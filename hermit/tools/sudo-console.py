#!/usr/bin/env python3
"""sudo-console — live HERMIT operator TUI.

No third-party dependencies. Reads the daemon's tmpfs telemetry and writes one
small control file; privileged lifecycle actions go through a fixed root-owned
helper installed by provision.sh.
"""

from __future__ import annotations

import curses
import json
import math
import os
import subprocess
import textwrap
import time
from collections import deque
from datetime import datetime
from pathlib import Path
from typing import Any

STATE_DIR = Path(os.environ.get("HERMIT_STATE_DIR", "/run/hermit-console"))
LIVE = STATE_DIR / "telemetry" / "live.json"
EVENTS = STATE_DIR / "telemetry" / "events.jsonl"
CONTROL = STATE_DIR / "control" / "control.json"
POWER_HELPER = os.environ.get("HERMIT_CONSOLE_POWER_HELPER", "/usr/local/sbin/hermit-console-power")
ALLOWED_ACTIONS = {"service-restart", "reboot", "poweroff"}


def read_json(path: Path) -> dict[str, Any]:
    try:
        return json.loads(path.read_text())
    except (OSError, json.JSONDecodeError, TypeError):
        return {}


def read_events(path: Path) -> list[dict[str, Any]]:
    try:
        lines = path.read_text().splitlines()[-500:]
    except OSError:
        return []
    out: list[dict[str, Any]] = []
    for line in lines:
        try:
            value = json.loads(line)
            if isinstance(value, dict):
                out.append(value)
        except (json.JSONDecodeError, TypeError):
            continue
    return out


def as_float(value: Any, default: float = 0.0) -> float:
    try:
        return float(value)
    except (TypeError, ValueError):
        return default


def bar(value: float, lo: float, hi: float, width: int) -> str:
    width = max(1, width)
    frac = max(0.0, min(1.0, (value - lo) / max(hi - lo, 1e-9)))
    filled = int(frac * width)
    return "█" * filled + "░" * (width - filled)


def spark(values: list[float], lo: float, hi: float, width: int) -> str:
    chars = "▁▂▃▄▅▆▇█"
    width = max(1, width)
    vals = values[-width:]
    if len(vals) < width:
        vals = [lo] * (width - len(vals)) + vals
    return "".join(chars[int(max(0, min(7, (v - lo) / max(hi - lo, 1e-9) * 7)))] for v in vals)


def fmt_event(event: dict[str, Any]) -> str:
    ts = datetime.fromtimestamp(as_float(event.get("ts"))).strftime("%H:%M:%S")
    kind = str(event.get("type", "?"))
    if kind == "transcript":
        role = str(event.get("role", "")).lower()
        who = "YOU " if role == "user" else "SUDO" if role == "assistant" else role.upper()[:4]
        return f"{ts}  {who:4}  {event.get('text', '')}"
    if kind == "ww_fired":
        return f"{ts}  WAKE  score={as_float(event.get('score')):.3f}"
    if kind == "manual_trigger":
        return f"{ts}  WAKE  manual trigger"
    if kind == "turn_complete":
        spoken = "spoken" if event.get("speech_completed") else "text-only"
        return f"{ts}  DONE  {spoken}"
    if kind == "no_speech":
        return f"{ts}  INFO  no speech captured"
    if kind == "turn_error":
        return f"{ts}  ERROR {event.get('error', 'turn failed')}"
    fields = {k: v for k, v in event.items() if k not in ("ts", "type")}
    return f"{ts}  {kind[:8]:8}  {json.dumps(fields, ensure_ascii=False)}"


def run_action(action: str) -> tuple[bool, str]:
    if action not in ALLOWED_ACTIONS:
        return False, f"action not allowed: {action}"
    try:
        result = subprocess.run(
            ["sudo", "-n", POWER_HELPER, action],
            capture_output=True,
            text=True,
            timeout=10,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        return False, str(exc)
    message = (result.stderr or result.stdout).strip()
    return result.returncode == 0, message or ("requested" if result.returncode == 0 else "failed")


class State:
    def __init__(self) -> None:
        self.mic_muted = False
        self.speaker_muted = False
        self.rms_history: deque[float] = deque(maxlen=240)
        self.ww_history: deque[float] = deque(maxlen=240)
        self.events: list[dict[str, Any]] = []
        self.live: dict[str, Any] = {}
        value = read_json(CONTROL)
        try:
            age = time.time() - float(value.get("ts", 0.0))
        except (TypeError, ValueError):
            return
        if 0.0 <= age <= 3.0:
            self.mic_muted = bool(value.get("mic_muted", False))
            self.speaker_muted = bool(value.get("speaker_muted", False))

    def update(self) -> None:
        self.live = read_json(LIVE)
        self.events = read_events(EVENTS)
        try:
            self.rms_history.append(float(self.live.get("rms", -99.0)))
        except (TypeError, ValueError):
            self.rms_history.append(-99.0)
        try:
            self.ww_history.append(float(self.live.get("ww", 0.0) or 0.0))
        except (TypeError, ValueError):
            self.ww_history.append(0.0)

    def write_control(self) -> None:
        CONTROL.parent.mkdir(parents=True, exist_ok=True)
        tmp = CONTROL.with_suffix(".tmp")
        tmp.write_text(
            json.dumps(
                {
                    "ts": time.time(),
                    "mic_muted": self.mic_muted,
                    "speaker_muted": self.speaker_muted,
                },
                separators=(",", ":"),
            )
        )
        os.chmod(tmp, 0o660)
        os.replace(tmp, CONTROL)

    @property
    def alive(self) -> bool:
        try:
            return time.time() - float(self.live.get("ts", 0)) < 3.0
        except (TypeError, ValueError):
            return False

    @property
    def conversation(self) -> list[dict[str, Any]]:
        return [e for e in self.events if e.get("type") == "transcript"]

    @property
    def activity(self) -> list[dict[str, Any]]:
        return [e for e in self.events if e.get("type") != "transcript"]


def safe_add(screen: Any, y: int, x: int, text: str, attr: int = 0) -> None:
    h, w = screen.getmaxyx()
    if y < 0 or y >= h or x < 0 or x >= w:
        return
    clipped = str(text)[: max(0, w - x - 1)]
    if not clipped:
        return
    try:
        screen.addstr(y, x, clipped, attr)
    except curses.error:
        pass


def wrapped_event_lines(events: list[dict[str, Any]], width: int, limit: int) -> list[str]:
    lines: list[str] = []
    for event in events:
        formatted = fmt_event(event)
        wrapped = textwrap.wrap(formatted, width=max(12, width), subsequent_indent="      ") or [""]
        lines.extend(wrapped)
    return lines[-limit:]


def draw(screen: Any, state: State, status: str, pending: tuple[str, float] | None) -> None:
    screen.erase()
    h, w = screen.getmaxyx()
    if h < 16 or w < 50:
        safe_add(screen, 0, 0, "sudo-console needs at least 50x16", curses.A_BOLD)
        safe_add(screen, 2, 0, "Resize terminal; q quits.")
        screen.refresh()
        return

    alive_attr = curses.color_pair(2) | curses.A_BOLD if state.alive else curses.color_pair(1) | curses.A_BOLD
    safe_add(screen, 0, 0, " SUDO / HERMIT CONSOLE ", curses.A_REVERSE | curses.A_BOLD)
    safe_add(screen, 0, max(25, w - 28), "● LIVE" if state.alive else "● DEAD / STALE", alive_attr)
    safe_add(
        screen,
        2,
        0,
        f"[m] mic {'MUTED' if state.mic_muted else 'open ':5}   "
        f"[s] speaker {'MUTED' if state.speaker_muted else 'open ':5}   "
        "[r] restart HERMIT  [b] reboot  [p] poweroff  [q] quit",
        curses.A_BOLD,
    )

    graph_w = max(10, w - 18)
    rms = as_float(state.live.get("rms"), -99.0)
    safe_add(screen, 4, 0, f"MIC  {rms:6.1f} dBFS ", curses.A_BOLD)
    safe_add(screen, 4, 17, bar(rms, -60, 0, graph_w), curses.color_pair(3))
    safe_add(screen, 5, 17, spark(list(state.rms_history), -60, 0, graph_w), curses.color_pair(3))

    ww = as_float(state.live.get("ww"), 0.0)
    threshold = as_float(state.live.get("ww_threshold"), 0.5) or 0.5
    listening = bool(state.live.get("listening", False))
    safe_add(
        screen,
        7,
        0,
        f"WAKE {ww:6.3f}/{threshold:.3f} " + ("LISTENING" if listening else "waiting  "),
        curses.A_BOLD,
    )
    safe_add(screen, 7, 25, bar(ww, 0, max(threshold * 1.5, 1e-3), max(10, w - 26)), curses.color_pair(4))
    safe_add(screen, 8, 25, spark(list(state.ww_history), 0, max(threshold * 1.5, 1e-3), max(10, w - 26)), curses.color_pair(4))

    interim = str(state.live.get("transcript_interim", "") or "")
    if interim:
        safe_add(screen, 9, 0, "HEARING: " + interim, curses.color_pair(4) | curses.A_BOLD)

    convo_top = 11
    activity_height = 4
    footer_rows = 2
    convo_height = max(3, h - convo_top - activity_height - footer_rows - 2)
    safe_add(screen, convo_top, 0, "CONVERSATION", curses.A_UNDERLINE | curses.A_BOLD)
    convo_lines = wrapped_event_lines(state.conversation, w - 2, convo_height)
    for idx, line in enumerate(convo_lines):
        attr = curses.color_pair(3) if "SUDO" in line else 0
        safe_add(screen, convo_top + 1 + idx, 0, line, attr)

    activity_top = convo_top + 1 + convo_height
    safe_add(screen, activity_top, 0, "ACTIVITY", curses.A_UNDERLINE | curses.A_BOLD)
    for idx, line in enumerate(wrapped_event_lines(state.activity, w - 2, activity_height)):
        safe_add(screen, activity_top + 1 + idx, 0, line)

    if pending and time.monotonic() < pending[1]:
        action = pending[0].replace("service-restart", "restart HERMIT")
        footer = f"Confirm {action}? [y] yes  [n/esc] cancel"
        attr = curses.color_pair(1) | curses.A_BOLD
    else:
        footer = status or f"state: {STATE_DIR}"
        attr = curses.A_DIM
    safe_add(screen, h - 1, 0, footer, attr)
    screen.refresh()


def tui(screen: Any) -> None:
    try:
        curses.curs_set(0)
    except curses.error:
        pass
    screen.nodelay(True)
    screen.timeout(125)
    try:
        if curses.has_colors():
            curses.start_color()
            curses.use_default_colors()
            curses.init_pair(1, curses.COLOR_RED, -1)
            curses.init_pair(2, curses.COLOR_GREEN, -1)
            curses.init_pair(3, curses.COLOR_CYAN, -1)
            curses.init_pair(4, curses.COLOR_YELLOW, -1)
    except curses.error:
        pass

    state = State()
    status = ""
    pending: tuple[str, float] | None = None
    control_next = 0.0
    while True:
        state.update()
        now = time.monotonic()
        if now >= control_next:
            try:
                state.write_control()
            except OSError as exc:
                status = f"control write failed: {exc}"
            control_next = now + 0.5
        if pending and time.monotonic() >= pending[1]:
            pending = None
            status = "confirmation expired"
        draw(screen, state, status, pending)
        key = screen.getch()
        if key in (ord("q"), ord("Q")):
            break
        if key == ord("m"):
            state.mic_muted = not state.mic_muted
            state.write_control()
            status = "microphone muted" if state.mic_muted else "microphone enabled"
        elif key == ord("s"):
            state.speaker_muted = not state.speaker_muted
            state.write_control()
            status = "speaker muted" if state.speaker_muted else "speaker enabled"
        elif key == ord("r"):
            pending = ("service-restart", time.monotonic() + 8)
        elif key == ord("b"):
            pending = ("reboot", time.monotonic() + 8)
        elif key == ord("p"):
            pending = ("poweroff", time.monotonic() + 8)
        elif key in (ord("n"), 27):
            pending = None
            status = "cancelled"
        elif key in (ord("y"), ord("Y")) and pending:
            action = pending[0]
            pending = None
            ok, message = run_action(action)
            status = ("OK: " if ok else "FAILED: ") + message
            if ok and action in ("reboot", "poweroff"):
                draw(screen, state, status, None)
                time.sleep(1)
                break

    state.mic_muted = False
    state.speaker_muted = False
    try:
        state.write_control()
    except OSError:
        pass


def main() -> int:
    if os.environ.get("TERM", "") in ("", "dumb", "unknown"):
        os.environ["TERM"] = "xterm-256color"
    try:
        STATE_DIR.mkdir(parents=True, exist_ok=True)
    except OSError as exc:
        print(f"sudo-console: cannot access {STATE_DIR}: {exc}")
        print("Re-run provision.sh and reconnect so group membership is refreshed.")
        return 1
    if not os.access(STATE_DIR, os.R_OK | os.X_OK) or not os.access(
        CONTROL.parent, os.W_OK | os.X_OK
    ):
        print(f"sudo-console: telemetry/control permissions are not ready for {os.getlogin()}")
        print("Re-run provision.sh and reconnect so group membership is refreshed.")
        return 1
    try:
        curses.wrapper(tui)
    except KeyboardInterrupt:
        return 130
    except curses.error as exc:
        print(f"sudo-console: terminal initialization failed: {exc}")
        print("Run from an interactive terminal or use: TERM=xterm-256color sudo-console")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
