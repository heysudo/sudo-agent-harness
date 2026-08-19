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
        # Console-requested volume. None until the operator touches -/+ so an
        # idle console never overrides voice commands ("Sudo, volume up").
        # A request lives until the daemon acknowledges it in live.json or it
        # expires (see update); it is never pinned indefinitely.
        self.volume: int | None = None
        self.volume_req_at = 0.0
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
        # Volume is a REQUEST, not a setting: the moment the daemon reports
        # the value we asked for, clear it and follow live truth again. Keeping
        # it pinned forever made the gauge ignore voice commands ("volume 85")
        # and left a stale volume in control.json that a daemon restart would
        # re-apply over whatever the user had set by voice since.
        if self.volume is not None:
            live_vol = self.live.get("volume")
            acked = live_vol is not None and int(as_float(live_vol, -1)) == self.volume
            # Expiry guards the race where a voice command lands in the same
            # window: after a few daemon polls without an ack, stop insisting.
            if acked or (time.time() - self.volume_req_at) > 4.0:
                self.volume = None
                self.write_control()  # drop the volume key from control.json
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
        payload: dict[str, Any] = {
            "ts": time.time(),
            "mic_muted": self.mic_muted,
            "speaker_muted": self.speaker_muted,
        }
        if self.volume is not None:
            payload["volume"] = self.volume
        tmp = CONTROL.with_suffix(".tmp")
        tmp.write_text(json.dumps(payload, separators=(",", ":")))
        os.chmod(tmp, 0o660)
        os.replace(tmp, CONTROL)

    def nudge_volume(self, delta: int) -> int:
        """Adjust requested volume from the daemon's live value on first touch."""
        if self.volume is None:
            base = int(as_float(self.live.get("volume"), 70.0))
        else:
            base = self.volume
        self.volume = max(0, min(100, base + delta))
        self.volume_req_at = time.time()
        return self.volume

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


def hline(screen: Any, y: int, w: int, label: str = "", attr: int = 0) -> None:
    """One horizontal rule, optionally with a section label set into it."""
    line = "─" * max(0, w - 1)
    safe_add(screen, y, 0, line, curses.A_DIM)
    if label:
        safe_add(screen, y, 2, f" {label} ", attr | curses.A_BOLD)


def draw(screen: Any, state: State, status: str, pending: tuple[str, float] | None) -> None:
    screen.erase()
    h, w = screen.getmaxyx()
    if h < 20 or w < 60:
        safe_add(screen, 0, 0, "sudo-console needs at least 60x20", curses.A_BOLD)
        safe_add(screen, 2, 0, "Resize terminal; q quits.")
        screen.refresh()
        return

    dim = curses.A_DIM
    cyan = curses.color_pair(3)
    yellow = curses.color_pair(4)

    # ── row 0: title bar ────────────────────────────────────────────────
    alive_attr = curses.color_pair(2) | curses.A_BOLD if state.alive else curses.color_pair(1) | curses.A_BOLD
    safe_add(screen, 0, 0, " SUDO / HERMIT CONSOLE ", curses.A_REVERSE | curses.A_BOLD)
    live_tag = "● LIVE" if state.alive else "● DEAD / STALE"
    safe_add(screen, 0, max(25, w - len(live_tag) - 2), live_tag, alive_attr)

    # ── controls ────────────────────────────────────────────────────────
    hline(screen, 1, w, "CONTROLS")
    mic = "MUTED" if state.mic_muted else "open"
    spk = "MUTED" if state.speaker_muted else "open"
    mic_attr = curses.color_pair(1) | curses.A_BOLD if state.mic_muted else curses.A_BOLD
    spk_attr = curses.color_pair(1) | curses.A_BOLD if state.speaker_muted else curses.A_BOLD
    safe_add(screen, 2, 2, f"[m] mic {mic:5}", mic_attr)
    safe_add(screen, 2, 19, f"[s] speaker {spk:5}", spk_attr)
    safe_add(screen, 2, 40, "[-/+] volume", curses.A_BOLD)
    safe_add(screen, 3, 2, "[r] restart HERMIT   [b] reboot   [p] poweroff   [q] quit", dim)

    # ── audio meters ────────────────────────────────────────────────────
    hline(screen, 4, w, "AUDIO")
    label_w = 18
    graph_w = max(10, w - label_w - 2)
    rms = as_float(state.live.get("rms"), -99.0)
    safe_add(screen, 5, 2, f"MIC  {rms:6.1f} dBFS", curses.A_BOLD)
    safe_add(screen, 5, label_w, bar(rms, -60, 0, graph_w), cyan)
    safe_add(screen, 6, label_w, spark(list(state.rms_history), -60, 0, graph_w), cyan | dim)

    ww = as_float(state.live.get("ww"), 0.0)
    threshold = as_float(state.live.get("ww_threshold"), 0.5) or 0.5
    listening = bool(state.live.get("listening", False))
    wake_tag = "LISTENING" if listening else "waiting"
    safe_add(screen, 7, 2, f"WAKE {ww:5.3f}/{threshold:.3f}", curses.A_BOLD)
    safe_add(screen, 7, label_w, bar(ww, 0, max(threshold * 1.5, 1e-3), graph_w), yellow)
    safe_add(screen, 8, label_w, spark(list(state.ww_history), 0, max(threshold * 1.5, 1e-3), graph_w), yellow | dim)
    safe_add(screen, 8, 2, wake_tag, yellow | curses.A_BOLD if listening else dim)

    # Volume gauge: pending console request wins the display; live value else.
    live_vol = state.live.get("volume")
    vol = state.volume if state.volume is not None else (int(as_float(live_vol, -1)) if live_vol is not None else None)
    if vol is None:
        safe_add(screen, 9, 2, "VOL    n/a", curses.A_BOLD)
        safe_add(screen, 9, label_w, "░" * graph_w, dim)
    else:
        tag = "*" if state.volume is not None and (live_vol is None or int(as_float(live_vol, -1)) != state.volume) else " "
        safe_add(screen, 9, 2, f"VOL  {vol:5d}%{tag}", curses.A_BOLD)
        safe_add(screen, 9, label_w, bar(float(vol), 0, 100, graph_w), curses.color_pair(2))

    interim = str(state.live.get("transcript_interim", "") or "")
    if interim:
        safe_add(screen, 10, 2, "HEARING: " + interim, yellow | curses.A_BOLD)

    # ── conversation / activity ─────────────────────────────────────────
    convo_top = 11
    activity_height = 4
    footer_rows = 2
    convo_height = max(3, h - convo_top - activity_height - footer_rows - 3)
    hline(screen, convo_top, w, "CONVERSATION")
    convo_lines = wrapped_event_lines(state.conversation, w - 4, convo_height)
    for idx, line in enumerate(convo_lines):
        attr = cyan if "SUDO" in line else 0
        safe_add(screen, convo_top + 1 + idx, 2, line, attr)

    activity_top = convo_top + 1 + convo_height
    hline(screen, activity_top, w, "ACTIVITY")
    for idx, line in enumerate(wrapped_event_lines(state.activity, w - 4, activity_height)):
        safe_add(screen, activity_top + 1 + idx, 2, line, dim)

    # ── footer ──────────────────────────────────────────────────────────
    hline(screen, h - 2, w)
    if pending and time.monotonic() < pending[1]:
        action = pending[0].replace("service-restart", "restart HERMIT")
        footer = f"Confirm {action}? [y] yes  [n/esc] cancel"
        attr = curses.color_pair(1) | curses.A_BOLD
    else:
        footer = status or f"state: {STATE_DIR}"
        attr = dim
    safe_add(screen, h - 1, 2, footer, attr)
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
        elif key in (ord("-"), ord("_"), curses.KEY_DOWN):
            status = f"volume {state.nudge_volume(-5)}%"
            state.write_control()
        elif key in (ord("+"), ord("="), curses.KEY_UP):
            status = f"volume {state.nudge_volume(+5)}%"
            state.write_control()
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
