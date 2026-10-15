"""Hyprland side of persistent voice mode: the kde_bridge.py JSON-lines protocol
(ops enable/disable/target/paste/submit/copy/shutdown, event "ready").

Only the desktop half differs: the focused window comes from Hyprland IPC
instead of a KWin script, and voice mode is toggled by a Hyprland bind running
`diktor toggle` instead of a KGlobalAccel shortcut. Clipboard
paste, the single Enter and the tmux route are kde_bridge.py's own guarded
functions, imported unchanged. While voice mode is on, focus follows the mouse
(input:follow_mouse = 1); the original value is restored on disable, shutdown
and, through the watchdog, after a crash. No microphone access."""
import json
import os
from pathlib import Path
import select
import shutil
import signal
import socket
import subprocess
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parent))
# desktop.rs writes kde_bridge.py next to this file.
from kde_bridge import (  # noqa: E402
    BridgeError, Deadline, PasteGate, TmuxRoute, alive, copy, guarded_paste,
    guarded_submit, log, same_target, send_keys)

HOVER = 1
# Original follow_mouse, written before it is changed so the watchdog can
# restore it when the helper dies without running its own cleanup.
RESTORE_FILE = "follow_mouse"


def control_socket():
    runtime = os.environ.get("XDG_RUNTIME_DIR") or f"/run/user/{os.getuid()}"
    return f"{runtime}/hypr/{os.environ['HYPRLAND_INSTANCE_SIGNATURE']}/.socket.sock"


def ipc(request, deadline=None, timeout=1.):
    """One request on Hyprland's control socket: the bytes hyprctl sends."""
    def budget():
        return min(timeout, deadline.remaining()) if deadline else timeout

    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as conn:
        conn.settimeout(budget())
        conn.connect(control_socket())
        conn.sendall(request.encode())
        reply = bytearray()
        while chunk := conn.recv(65536):
            reply.extend(chunk)
            conn.settimeout(budget())
    return reply.decode(errors="replace")


def target_of(window, monitors):
    """The fields the KWin report carries, from `j/activewindow` and `j/monitors`.
    No focused window gives an empty id, which same_target() rejects."""
    output = next((m for m in monitors if m.get("id") == window.get("monitor")), {})
    return {
        "id": str(window.get("address") or ""),
        "app_id": str(window.get("class") or window.get("initialClass") or ""),
        "pid": max(int(window.get("pid") or 0), 0),
        "output": str(output.get("name") or ""),
        "output_x": int(output.get("x") or 0),
        "output_y": int(output.get("y") or 0),
    }


def set_follow_mouse(value, deadline=None):
    # Lua configs refuse `keyword`; `eval` applies the option at runtime.
    reply = ipc(f"eval hl.config({{ input = {{ follow_mouse = {int(value)} }} }})", deadline).strip()
    if reply != "ok":
        raise BridgeError(f"follow_mouse: {reply}")


def restore_hover(path, deadline=None):
    """Put the saved follow_mouse back. The file stays on failure, so the
    watchdog tries again; idempotent, the helper and watchdog may both run it."""
    try:
        original = int(path.read_text())
    except (OSError, ValueError):
        return
    try:
        set_follow_mouse(original, deadline)
    except Exception as error:
        log(f"follow_mouse restore to {original} failed: {error!r}")
        return
    path.unlink(missing_ok=True)
    log(f"follow_mouse restored to {original}")


def hover_on(path, deadline):
    """Focus follows the mouse while voice mode is on. A failure only logs:
    hover is a convenience, every paste still checks the focused window."""
    if path.exists():
        return  # Already on; reading the option now would save our own value.
    try:
        original = json.loads(ipc("j/getoption input:follow_mouse", deadline))["int"]
        path.write_text(str(int(original)))
        if original != HOVER:
            set_follow_mouse(HOVER, deadline)
            log(f"follow_mouse {original} -> {HOVER} while voice mode is on")
    except Exception as error:
        log(f"follow_mouse not changed: {error!r}")


def exit_on_sigterm():
    # systemd stops the whole unit with SIGTERM; unwind so `finally` restores.
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))


def watchdog(directory, parent, owner):
    """Independent process: survives helper/app SIGKILL and a hung helper."""
    exit_on_sigterm()
    (directory / "watch-ready").touch()
    try:
        while alive(parent) and alive(owner):
            try:
                if time.monotonic() - float((directory / "heartbeat").read_text()) > 4:
                    break
            except (FileNotFoundError, ValueError):
                pass
            time.sleep(.05)
    finally:
        try:
            (directory / "generation").write_text("0")
        except OSError:
            pass
        if alive(owner):
            try:
                os.kill(owner, signal.SIGKILL)
            except ProcessLookupError:
                pass
        restore_hover(directory / RESTORE_FILE)
        shutil.rmtree(directory, ignore_errors=True)


def main(directory, parent):
    exit_on_sigterm()
    hover = directory / RESTORE_FILE
    enabled = False
    generation = 0
    running = True
    gate = PasteGate()
    last_beat = 0.

    def emit(value):
        print(json.dumps(value, ensure_ascii=False), flush=True)

    def heartbeat():
        nonlocal last_beat
        now = time.monotonic()
        if now - last_beat >= .25:
            (directory / "heartbeat").write_text(str(now))
            last_beat = now

    def pump(_deadline):
        # Focus is queried, never pushed, so there is nothing to dispatch while
        # waiting on a subprocess; keep the watchdog fed through a long paste.
        heartbeat()

    route = TmuxRoute(pump)

    def target_now(deadline):
        try:
            window = json.loads(ipc("j/activewindow", deadline))
            monitors = json.loads(ipc("j/monitors", deadline))
        except (OSError, ValueError, KeyError) as error:
            # Nothing was touched yet: a clean refusal, not an uncertain delivery.
            raise BridgeError("target_unavailable") from error
        target = target_of(window, monitors)
        if not same_target(target, target):
            # No window focused at all, or one without class or pid.
            log(f"no usable focused window: address={window.get('address')!r} "
                f"class={window.get('class')!r} pid={window.get('pid')!r}")
            raise BridgeError("target_unavailable")
        return target

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

    # Requests are handled strictly one after another (no nested dispatch as
    # in the GLib helper), so the gate is never found busy at entry here.
    def command(request):
        nonlocal enabled, generation, running
        op = request["op"]
        if op == "enable":
            deadline = Deadline()
            generation = int(request["generation"])
            enabled = True
            hover_on(hover, deadline)
            return target_now(deadline)
        if op == "disable":
            enabled = False
            restore_hover(hover, Deadline())
            return None
        if op == "target":
            return target_now(Deadline())
        if op == "paste":
            return guarded_paste(request, current, target_now, copy_text, send, gate,
                                 pump, Deadline(), route)
        if op == "submit":
            return guarded_submit(request, current, target_now, send, gate, Deadline(), route)
        if op == "copy":
            copy_text(request["text"], Deadline())
            return None
        if op == "shutdown":
            enabled = False
            running = False
            restore_hover(hover, Deadline())
            return None
        raise BridgeError("desktop")

    heartbeat()
    watch = subprocess.Popen([sys.executable, __file__, "watchdog", str(directory), str(parent), str(os.getpid())],
                             start_new_session=True, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL)
    try:
        deadline = time.monotonic() + 3
        while not (directory / "watch-ready").exists():
            if watch.poll() is not None or time.monotonic() > deadline:
                raise BridgeError("desktop")
            time.sleep(.02)
        emit({"event": "ready"})
        buffer = bytearray()
        while running:
            heartbeat()
            if not select.select([0], [], [], .25)[0]:
                continue
            data = os.read(0, 65536)
            if not data:
                break
            buffer.extend(data)
            if len(buffer) > 262144:
                break
            while running and b"\n" in buffer:
                line, _, rest = buffer.partition(b"\n")
                buffer[:] = rest
                request = {}
                try:
                    request = json.loads(line)
                    result = command(request)
                    emit({"id": request["id"], "result": result})
                except Exception as error:
                    code = str(error) if isinstance(error, BridgeError) else "desktop"
                    print(f"voice helper: {error}", file=sys.stderr)
                    emit({"id": request.get("id"), "error": code})
    finally:
        enabled = False
        restore_hover(hover)


if __name__ == "__main__":
    if sys.argv[1] == "watchdog":
        watchdog(Path(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4]))
    else:
        main(Path(sys.argv[1]), int(sys.argv[2]))
