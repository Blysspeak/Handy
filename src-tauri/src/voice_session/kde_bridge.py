"""Transient KDE shortcuts and guarded clipboard paste. No microphone access."""
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import time
import unicodedata

KEYS = {"toggle": 335544388}
ACCEL = "org.kde.KGlobalAccel"
TMUX_BUFFER = "handy-voice"
# Claude Code collapses a long paste into "[Pasted text #N ...]" (screen_key form).
PASTED_MARKER = "[Pastedtext"
POLL = .05
PASTE_CONFIRM = 1.
SUBMIT_CONFIRM = 1.5


class BridgeError(Exception):
    pass


class Deadline:
    """One budget below Rust's 3s request timeout for desktop delivery."""
    def __init__(self):
        self.end = time.monotonic() + 2.5
        self.reserve = .4
        self.step_end = None

    def remaining(self, reserve=None):
        left = self.end - time.monotonic() - (self.reserve if reserve is None else reserve)
        if self.step_end is not None:
            left = min(left, self.step_end - time.monotonic())
        if left <= 0:
            raise BridgeError("timeout")
        return left


class PasteGate:
    def __init__(self):
        self.busy = False


class FocusReports:
    """Only a reply bearing the current query nonce can prove fresh focus."""
    def __init__(self):
        self.focus = {}
        self.serial = 0
        self.pending = None
        self.reply = None

    def begin(self):
        if self.pending is not None:
            raise BridgeError("busy")
        self.serial += 1
        self.pending = self.serial
        self.reply = None
        return self.serial

    def report(self, value):
        nonce = value.get("nonce")
        if nonce is None:
            self.focus = value
        elif nonce == self.pending:
            self.reply = value

    def end(self):
        self.pending = None
        self.reply = None


def sanitize(text):
    return "".join(" " if unicodedata.category(c) in ("Cc", "Cf", "Zl", "Zp") else c for c in text)


def same_target(left, right):
    return bool(left and right and left.get("id") and left.get("app_id") and left.get("pid")) and all(
        left.get(key) == right.get(key) for key in ("id", "app_id", "pid")
    )


def paste_keys(app_id):
    if "ghostty" in app_id.lower():
        return ["135:1", "135:0"]  # Dedicated Paste key is independent of layout.
    # Shift+Insert (evdev 42, 110) is layout-independent and universally pastes
    # from clipboard in GUI apps and primary selection in terminals without layout interference.
    return ["42:1", "110:1", "110:0", "42:0"]


def log(message):
    print(f"voice delivery: {message}", file=sys.stderr, flush=True)


def screen_key(text):
    """Text without whitespace or box drawing: TUI wrapping and borders drop out."""
    return "".join(c for c in text if not c.isspace() and not "─" <= c <= "╿")


def needles(text):
    key = screen_key(text)
    return ([key[-24:]] if key else []) + [PASTED_MARKER]


def depth(screen, needle):
    """Fewest bottom content rows of a pane capture that contain needle, or None."""
    joined = ""
    for count, row in enumerate(reversed(screen.rstrip().split("\n")), 1):
        joined = screen_key(row) + joined
        if needle in joined:
            return count
    return None


def shown(before, after, candidates):
    """The first needle that is new, or lower, in the pane after a paste."""
    if after is None or after == before:
        return None
    for needle in candidates:
        now = depth(after, needle)
        if now is not None:
            was = depth(before, needle)
            if was is None or now < was:
                return needle
    return None


def descends(pid, ancestor):
    """True when pid runs below ancestor (a tmux client inside the Ghostty window)."""
    for _ in range(64):
        if pid == ancestor:
            return True
        if pid <= 1:
            return False
        try:
            pid = int(Path(f"/proc/{pid}/stat").read_text().rsplit(") ", 1)[1].split()[1])
        except (OSError, IndexError, ValueError):
            return False
    return False


class TmuxRoute:
    """Pty delivery into the tmux pane of the one focused client inside the
    target Ghostty; remembers the pane and needle of the generation's last paste."""
    def __init__(self, pump):
        self.pump = pump
        self.last = None

    def run(self, deadline, *args, data=None, capture=False):
        out = run_command(["tmux", *args], deadline, self.pump, data, capture=capture)
        return out.decode(errors="replace") if capture else None

    def resolve(self, target, deadline):
        """(pane, None) or (None, reason to fall back to ydotool)."""
        if "ghostty" not in str(target.get("app_id", "")).lower():
            return None, "not_ghostty"
        if shutil.which("tmux") is None:
            return None, "no_tmux"
        try:
            listing = self.run(deadline, "list-clients", "-F", "#{client_flags}\t#{pane_id}\t#{client_pid}",
                               capture=True)
        except (OSError, subprocess.CalledProcessError):
            return None, "no_tmux_server"
        focused = []
        for line in listing.splitlines():
            flags, pane, pid = (line.split("\t") + ["", ""])[:3]
            if "focused" in flags.split(","):
                focused.append((pane, pid))
        if len(focused) != 1:
            return None, f"focused_clients:{len(focused)}"
        pane, pid = focused[0]
        # A focused client in another terminal (ssh, phone) must not get the text.
        if not pane.startswith("%") or not pid.isdigit() or not descends(int(pid), int(target["pid"])):
            return None, "client_outside_target"
        return pane, None

    def capture(self, pane, deadline):
        return self.run(deadline, "capture-pane", "-p", "-t", pane, capture=True)

    def remembered(self, generation):
        return self.last if self.last and self.last["generation"] == generation else None

    def watch(self, pane, deadline, bound, done, live):
        """Poll the pane until done(screen), Off or the bound. Observation only:
        input is already delivered, so this never raises."""
        start = time.monotonic()
        screen = None
        try:
            end = start + min(bound, deadline.end - deadline.reserve - start - .1)
            while True:
                screen = self.capture(pane, deadline)
                if done(screen):
                    return screen, time.monotonic() - start, True
                if not live() or time.monotonic() + POLL > end:
                    break
                until = time.monotonic() + POLL
                while time.monotonic() < until:
                    self.pump(deadline)
                    time.sleep(.005)
        except Exception as error:
            log(f"watch stopped pane={pane}: {error!r}")
        return screen, time.monotonic() - start, False


def bottom(screen, limit=300):
    rows = [row.rstrip() for row in (screen or "").rstrip().split("\n") if row.strip()]
    return "\n".join(rows)[-limit:]


def copy(text, deadline, pump=None):
    data = sanitize(text).encode()
    p = pump or (lambda _d: None)
    run_command(["wl-copy", "--type", "text/plain;charset=utf-8"], deadline, p, data)
    run_command(["wl-copy", "--primary", "--type", "text/plain;charset=utf-8"], deadline, p, data)


def guarded_paste(request, current, target_now, copy, send, gate, pump, deadline=None, route=None):
    text = sanitize(request["text"])
    if not text.strip():
        return None
    if gate.busy:
        raise BridgeError("busy")
    # Set BEFORE any nested GLib dispatch in focus queries, subprocesses or waits.
    gate.busy = True
    deadline = deadline or Deadline()
    injected = False
    error = None
    result = None
    generation = request["generation"]

    def live():
        deadline.remaining()
        if not current(generation):
            raise BridgeError("cancelled")

    def check():
        live()
        if not same_target(request["target"], target_now(deadline)):
            raise BridgeError("target_changed")
        live()

    try:
        check()  # A mismatch here MUST NOT touch the clipboard or a tmux pane.
        pane, reason = route.resolve(request["target"], deadline) if route else (None, None)
        if pane is not None:
            check()
            if route.resolve(request["target"], deadline)[0] != pane:
                raise BridgeError("target_changed")
            before = route.capture(pane, deadline)
            route.run(deadline, "load-buffer", "-b", TMUX_BUFFER, "-", data=text.encode())
            live()
            injected = True
            route.run(deadline, "paste-buffer", "-p", "-d", "-b", TMUX_BUFFER, "-t", pane)
            # No Enter here either. Submit is allowed only once the text is seen.
            candidates = needles(text)
            screen, elapsed, confirmed = route.watch(
                pane, deadline, PASTE_CONFIRM,
                lambda screen: shown(before, screen, candidates) is not None,
                lambda: current(generation))
            route.last = {"generation": generation, "pane": pane, "candidates": candidates,
                          "needle": shown(before, screen, candidates)}
            visible_ms = round(elapsed * 1000)
            log(f"op=paste path=tmux pane={pane} confirmed={confirmed} visible_ms={visible_ms}")
            if not confirmed:
                log(f"paste unconfirmed pane={pane} bottom={json.dumps(bottom(screen), ensure_ascii=False)}")
            result = {"path": "tmux", "pane": pane, "confirmed": confirmed, "visible_ms": visible_ms}
        else:
            if route is not None:
                log(f"op=paste path=ydotool reason={reason}")
            copy(text, deadline)
            check()
            injected = True  # Even a failed subprocess may have emitted some input.
            send(paste_keys(request["target"]["app_id"]), deadline)
            # No Enter here: Rust submits the whole batch after 3 s of silence.
            # The gap only lets the app read this clipboard before the next copy.
            until = time.monotonic() + .08
            while time.monotonic() < until:
                live()
                pump(deadline)
                time.sleep(min(.002, deadline.remaining()))
            live()
            if route is not None:
                route.last = {"generation": generation, "pane": None}
            result = {"path": "ydotool"}
    except Exception as failure:
        error = failure
    finally:
        gate.busy = False
    if error is not None:
        if injected:
            raise BridgeError("delivery_uncertain") from error
        raise error
    return result


def guarded_submit(request, current, target_now, send, gate, deadline=None, route=None):
    """One Enter for already pasted text, only into the window (and tmux pane) that got it."""
    if gate.busy:
        raise BridgeError("busy")
    gate.busy = True
    deadline = deadline or Deadline()
    injected = False
    error = None
    result = None
    generation = request["generation"]

    def live():
        deadline.remaining()
        if not current(generation):
            raise BridgeError("cancelled")

    try:
        live()
        if not same_target(request["target"], target_now(deadline)):
            raise BridgeError("target_changed")
        live()
        last = route.remembered(generation) if route else None
        pane = last["pane"] if last else None
        if pane is not None:
            focused, reason = route.resolve(request["target"], deadline)
            live()
            if focused != pane:
                log(f"op=submit path=tmux pane={focused} expected={pane} reason={reason} result=target_changed")
                raise BridgeError("target_changed")
            before = route.capture(pane, deadline)
            candidates = [last["needle"]] if last["needle"] else last["candidates"]
            needle = next((n for n in candidates if depth(before, n) is not None), None)
            if needle is None:
                # Enter into a TUI still digesting the paste can be swallowed.
                log(f"op=submit path=tmux pane={pane} result=paste_unconfirmed "
                    f"bottom={json.dumps(bottom(before), ensure_ascii=False)}")
                raise BridgeError("paste_unconfirmed")
            rows = depth(before, needle)
            live()
            injected = True
            route.run(deadline, "send-keys", "-t", pane, "Enter")

            def cleared(screen):
                # Sent: the text left the input line, i.e. it is gone or now sits
                # at least two rows higher (above the prompt row and its border).
                # An Enter turned into a newline moves it up by only one row.
                now = depth(screen, needle)
                return screen != before and (now is None or now >= rows + 2)

            # Observe only. A second Enter could send an empty or duplicate message.
            screen, elapsed, confirmed = route.watch(pane, deadline, SUBMIT_CONFIRM, cleared,
                                                     lambda: current(generation))
            cleared_ms = round(elapsed * 1000)
            if confirmed:
                log(f"op=submit path=tmux pane={pane} confirmed=True cleared_ms={cleared_ms}")
            else:
                log(f"submit unconfirmed pane={pane} after_ms={cleared_ms} "
                    f"bottom={json.dumps(bottom(screen), ensure_ascii=False)}")
            result = {"path": "tmux", "pane": pane, "confirmed": confirmed, "cleared_ms": cleared_ms}
        else:
            if route is not None:
                log(f"op=submit path=ydotool reason={'last_paste_ydotool' if last else 'no_paste_record'}")
            injected = True
            send(["28:1", "28:0"], deadline)
            result = {"path": "ydotool"}
    except Exception as failure:
        error = failure
    finally:
        gate.busy = False
    if error is not None:
        if injected:
            raise BridgeError("delivery_uncertain") from error
        raise error
    return result


def run_command(argv, deadline, pump, data=None, duration=None, capture=False):
    """Returns stdout bytes when capture is set (its stderr is then discarded)."""
    deadline.remaining()
    until = time.monotonic() + duration if duration is not None else deadline.end
    previous_step = deadline.step_end
    deadline.step_end = min(previous_step or until, until)
    process = None
    try:
        process = subprocess.Popen(argv, stdin=subprocess.PIPE if data is not None else subprocess.DEVNULL,
                                   stdout=subprocess.PIPE if capture else subprocess.DEVNULL,
                                   stderr=subprocess.DEVNULL if capture else None)
        while True:
            try:
                left = min(deadline.remaining(), until - time.monotonic())
                if left <= 0:
                    raise BridgeError("timeout")
                output = process.communicate(input=data, timeout=min(.02, left))
                if process.returncode:
                    raise subprocess.CalledProcessError(process.returncode, argv)
                return output[0] if capture else None
            except subprocess.TimeoutExpired:
                data = None
                pump(deadline)
    finally:
        # The same slice bounds nested focus/control dispatch while input is
        # being emitted. Reaping and release-only recovery use the reserved
        # total budget.
        deadline.step_end = previous_step
        if process is not None and process.poll() is None:
            process.kill()
            process.wait(timeout=deadline.remaining(0))


def send_keys(keys, deadline, pump):
    try:
        # Bound a partial keydown well below normal keyboard autorepeat delay.
        run_command(["ydotool", "key", "--key-delay", "12", *keys], deadline, pump, duration=.15)
    except Exception:
        # A nonzero exit/timeout may follow a keydown. Release only; never retry
        # a press, even when Off or focus change cancelled the transaction.
        releases = [key.split(":")[0] + ":0" for key in keys if key.endswith(":1")]
        reserve = deadline.reserve
        deadline.reserve = 0
        try:
            run_command(["ydotool", "key", "--key-delay", "0", *releases], deadline, pump, duration=.15)
        finally:
            deadline.reserve = reserve
        raise


def alive(pid):
    try:
        return Path(f"/proc/{pid}/stat").read_text().split(") ", 1)[1][0] != "Z"
    except (FileNotFoundError, ProcessLookupError):
        return False


def connect():
    import dbus
    bus = dbus.SessionBus()
    accel = dbus.Interface(bus.get_object("org.kde.kglobalaccel", "/kglobalaccel"), ACCEL)
    return bus, accel


def action(component, key):
    import dbus
    return dbus.Array([component, "_k_session:" + key, "Handy voice", key], signature="s")


def cleanup(accel, component, keys=KEYS):
    for key in keys:
        try:
            accel.setInactive(action(component, key), timeout=1)
            accel.unRegister(action(component, key), timeout=1)
        except Exception as error:
            print(f"shortcut cleanup: {error}", file=sys.stderr)


def watchdog(directory, parent, owner, component):
    # Independent process: survives application/helper SIGKILL and hung D-Bus.
    import dbus
    bus, accel = connect()
    (directory / "watch-ready").touch()
    while alive(parent) and alive(owner):
        try:
            heartbeat = float((directory / "heartbeat").read_text())
            if time.monotonic() - heartbeat > 4:
                break
        except (FileNotFoundError, ValueError):
            pass
        time.sleep(.05)
    (directory / "generation").write_text("0")
    if alive(owner):
        try:
            os.kill(owner, signal.SIGKILL)
        except ProcessLookupError:
            pass
    cleanup(accel, component)
    scripting = dbus.Interface(bus.get_object("org.kde.KWin", "/Scripting"), "org.kde.kwin.Scripting")
    for suffix in ("observe", "query"):
        try:
            scripting.unloadScript(component + "_" + suffix, timeout=1)
        except Exception:
            pass
    shutil.rmtree(directory, ignore_errors=True)


def main(directory, parent):
    import dbus
    import dbus.service
    from dbus.mainloop.glib import DBusGMainLoop
    from gi.repository import GLib

    DBusGMainLoop(set_as_default=True)
    bus, accel = connect()
    component = "handy_voice_" + str(os.getpid())
    scripting = dbus.Interface(bus.get_object("org.kde.KWin", "/Scripting"), "org.kde.kwin.Scripting")
    loop = GLib.MainLoop()
    focus_reports = FocusReports()
    enabled = False
    generation = 0
    gate = PasteGate()
    transaction_deadline = None

    def emit(value):
        print(json.dumps(value, ensure_ascii=False), flush=True)

    class Focus(dbus.service.Object):
        @dbus.service.method("org.handy.Voice", in_signature="s", out_signature="")
        def Report(self, value):
            focus_reports.report(json.loads(str(value)))

    service = dbus.service.BusName("org.handy.Voice.p" + str(os.getpid()), bus)
    service_object = Focus(service, "/voice")
    def report_js(nonce=None):
        return """function report() {
        const w = workspace.activeWindow;
        const o = w ? w.output : null;
        callDBus(%s, '/voice', 'org.handy.Voice', 'Report', JSON.stringify({
            nonce: %s,
            id: w ? String(w.internalId) : '',
            app_id: w ? String(w.desktopFileName || w.resourceClass || w.resourceName) : '',
            pid: w ? w.pid : 0, output: o ? o.name : '',
            output_x: o ? o.geometry.x : 0, output_y: o ? o.geometry.y : 0
        }));
    } report();""" % (json.dumps(service.get_name()), json.dumps(nonce))

    def load_script(suffix, source, deadline):
        name = component + "_" + suffix
        scripting.unloadScript(name, timeout=min(1, deadline.remaining()))
        path = directory / (suffix + ".js")
        path.write_text(source)
        sid = int(bus.call_blocking("org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting", "loadScript", "ss", (str(path), name), timeout=min(1, deadline.remaining())))
        if sid < 0:
            raise BridgeError("target_unavailable")
        dbus.Interface(bus.get_object("org.kde.KWin", "/Scripting/Script" + str(sid)), "org.kde.kwin.Script").run(timeout=min(1, deadline.remaining()))

    def pump(deadline):
        context = GLib.MainContext.default()
        while context.pending():
            deadline.remaining()
            context.iteration(False)

    route = TmuxRoute(pump)

    def target_now(deadline):
        nonce = focus_reports.begin()
        try:
            load_script("query", report_js(nonce), deadline)
            until = time.monotonic() + min(.7, deadline.remaining())
            while focus_reports.reply is None and time.monotonic() < until:
                pump(deadline)
                time.sleep(min(.002, deadline.remaining()))
            target = focus_reports.reply
            if not same_target(target, target):
                raise BridgeError("target_unavailable")
            return dict(target)
        finally:
            focus_reports.end()

    def current(expected):
        try:
            return enabled and generation == expected and int((directory / "generation").read_text()) == expected and alive(parent)
        except (OSError, ValueError):
            return False

    def copy_text(text, deadline):
        copy(text, deadline, pump)

    def send(keys, deadline):
        if not current(generation):
            raise BridgeError("cancelled")
        send_keys(keys, deadline, pump)

    def register(keys, deadline):
        for key in keys:
            if not accel.isGlobalShortcutAvailable(dbus.Int32(KEYS[key]), "", timeout=deadline.remaining()):
                raise BridgeError("keys_unavailable")
        for key in keys:
            accel.doRegister(action(component, key), timeout=deadline.remaining())
            assigned = list(map(int, accel.setShortcut(action(component, key), dbus.Array([KEYS[key]], signature="i"), dbus.UInt32(6), timeout=deadline.remaining())))
            if assigned != [KEYS[key]]:
                raise BridgeError("keys_unavailable")

    def pressed(_component, key, _timestamp):
        nonlocal enabled
        key = str(key).removeprefix("_k_session:")
        if key == "toggle":
            # Cancel locally before the round trip to Rust, including during paste.
            enabled = False
            emit({"event": "toggle", "target": focus_reports.focus})

    def command(request):
        nonlocal enabled, generation, transaction_deadline
        op = request["op"]
        if op == "enable":
            if gate.busy:
                raise BridgeError("busy")
            deadline = Deadline()
            generation = int(request["generation"])
            enabled = True
            return target_now(deadline)
        if op == "disable":
            enabled = False
            return None
        if op == "target":
            return target_now(transaction_deadline or Deadline())
        if op == "paste":
            if gate.busy:
                raise BridgeError("busy")
            transaction_deadline = Deadline()
            try:
                return guarded_paste(request, current, target_now, copy_text, send, gate,
                                     pump, transaction_deadline, route)
            finally:
                transaction_deadline = None
        if op == "submit":
            if gate.busy:
                raise BridgeError("busy")
            transaction_deadline = Deadline()
            try:
                return guarded_submit(request, current, target_now, send, gate,
                                      transaction_deadline, route)
            finally:
                transaction_deadline = None
        if op == "copy":
            if gate.busy:
                raise BridgeError("busy")
            copy_text(request["text"], Deadline())
            return None
        if op == "shutdown":
            enabled = False
            loop.quit()
            return None
        raise BridgeError("desktop")

    input_buffer = bytearray()

    def read_input(_source, condition):
        if condition & GLib.IO_HUP:
            loop.quit()
            return False
        data = os.read(0, 65536)
        if not data:
            loop.quit()
            return False
        input_buffer.extend(data)
        if len(input_buffer) > 262144:
            loop.quit()
            return False
        while b"\n" in input_buffer:
            line, _, rest = input_buffer.partition(b"\n")
            input_buffer[:] = rest
            request = {}
            try:
                request = json.loads(line)
                result = command(request)
                emit({"id": request["id"], "result": result})
            except Exception as error:
                code = str(error) if isinstance(error, BridgeError) else "desktop"
                print(f"voice helper: {error}", file=sys.stderr)
                emit({"id": request.get("id"), "error": code})
        return True

    def heartbeat():
        (directory / "heartbeat").write_text(str(time.monotonic()))
        return True

    heartbeat()
    watch = subprocess.Popen([sys.executable, __file__, "watchdog", str(directory), str(parent), str(os.getpid()), component], start_new_session=True, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL)
    try:
        deadline = time.monotonic() + 3
        while not (directory / "watch-ready").exists():
            if watch.poll() is not None or time.monotonic() > deadline:
                raise BridgeError("desktop")
            time.sleep(.02)
        load_script("observe", report_js() + " workspace.windowActivated.connect(report);", Deadline())
        register(("toggle",), Deadline())
        component_path = accel.getComponent(component, timeout=1)
        shortcut_component = dbus.Interface(bus.get_object("org.kde.kglobalaccel", str(component_path)), "org.kde.kglobalaccel.Component")
        bus.add_signal_receiver(pressed, signal_name="globalShortcutPressed", dbus_interface="org.kde.kglobalaccel.Component", path=str(component_path))
        GLib.io_add_watch(0, GLib.IO_IN | GLib.IO_HUP, read_input)
        GLib.timeout_add(250, heartbeat)
        emit({"event": "ready"})
        loop.run()
    finally:
        enabled = False
        cleanup(accel, component)
        for suffix in ("observe", "query"):
            scripting.unloadScript(component + "_" + suffix, timeout=1)
        # Keep D-Bus service object referenced until scripts have been removed.
        del service_object


if __name__ == "__main__":
    if sys.argv[1] == "watchdog":
        watchdog(Path(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4]), sys.argv[5])
    else:
        main(Path(sys.argv[1]), int(sys.argv[2]))
