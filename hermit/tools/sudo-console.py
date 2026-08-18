#!/usr/bin/env python3
"""sudo-console — live TUI for the HERMIT voice device.

Borrowed from the b2-34 sudo-console design: reads the state files the daemon
writes to tmpfs and renders meters; the ONLY thing it writes is control.json
(mic/speaker mute), which the daemon polls at 8 Hz. It never holds a connection
to the daemon, so starting, stopping or crashing this console cannot disturb
audio — and with no console running, nothing is muted.

    sudo-console                 # attach to the running daemon (/run/hermit)
    HERMIT_STATE_DIR=... sudo-console   # attach to a dev instance

Keys:
    m   toggle microphone mute      (frames dropped before wake word and STT)
    s   toggle speaker mute         (audio plays as silence; capture stays clocked)
    c   clear the event pane
    q   quit

Panes: mic level meter + history, wake score graph with threshold line, wake
flashes, and the recent event/turn feed.

stdlib only (curses + json). No third-party deps, no venv, no pip.
"""

from __future__ import annotations

import curses
import json
import os
import time
from collections import deque
from pathlib import Path

STATE_DIR = Path(os.environ.get("HERMIT_STATE_DIR", "/run/hermit"))
SPARK = " ▁▂▃▄▅▆▇█"
STALE_S = 3.0     # live.json older than this => daemon not running
FLASH_S = 3.0     # how long a wake stays highlighted
HISTORY = 240     # samples kept for the graphs (~30 s at 8 Hz)


class State:
    """Tails live.json + events.jsonl; writes control.json."""

    def __init__(self) -> None:
        self.live: dict = {}
        self.events: deque = deque(maxlen=200)
        self.rms_hist: deque = deque(maxlen=HISTORY)
        self.ww_hist: deque = deque(maxlen=HISTORY)
        self.last_wake: float = 0.0
        self.mic_muted = False
        self.speaker_muted = False
        self._pos = 0
        self._inode = None

    def poll(self) -> None:
        try:
            self.live = json.loads((STATE_DIR / "live.json").read_text())
        except (OSError, ValueError):
            pass
        if self.alive:
            rms = self.live.get("rms")
            ww = self.live.get("ww")
            self.rms_hist.append(rms if isinstance(rms, (int, float)) else -99.0)
            self.ww_hist.append(ww if isinstance(ww, (int, float)) else 0.0)
        path = STATE_DIR / "events.jsonl"
        try:
            st = path.stat()
        except OSError:
            return
        if self._inode != st.st_ino or st.st_size < self._pos:
            self._inode, self._pos = st.st_ino, 0
        if st.st_size == self._pos:
            return
        try:
            with path.open("r", encoding="utf-8") as f:
                f.seek(self._pos)
                chunk = f.read()
                self._pos = f.tell()
        except OSError:
            return
        for line in chunk.splitlines():
            try:
                ev = json.loads(line)
            except ValueError:
                continue
            self.events.append(ev)
            if ev.get("type") == "ww_fired":
                self.last_wake = ev.get("ts", time.time())

    @property
    def alive(self) -> bool:
        ts = self.live.get("ts", 0)
        return bool(ts) and (time.time() - ts) < STALE_S

    def write_control(self) -> None:
        tmp = STATE_DIR / "control.json.tmp"
        try:
            tmp.write_text(json.dumps({
                "ts": time.time(),
                "mic_muted": self.mic_muted,
                "speaker_muted": self.speaker_muted,
            }))
            tmp.replace(STATE_DIR / "control.json")
        except OSError:
            pass


def spark(values, lo, hi, width) -> str:
    """Render the last `width` values as a sparkline between lo..hi."""
    vals = list(values)[-width:]
    out = []
    for v in vals:
        f = 0.0 if hi <= lo else max(0.0, min(1.0, (v - lo) / (hi - lo)))
        out.append(SPARK[round(f * (len(SPARK) - 1))])
    return "".join(out).rjust(width)


def bar(v, lo, hi, width) -> str:
    f = 0.0 if hi <= lo else max(0.0, min(1.0, (v - lo) / (hi - lo)))
    n = round(f * width)
    return "█" * n + "░" * (width - n)


def fmt_event(ev: dict) -> str:
    t = time.strftime("%H:%M:%S", time.localtime(ev.get("ts", 0)))
    typ = ev.get("type", "?")
    if typ == "ww_fired":
        return f"{t}  WAKE  score={ev.get('score', 0):.3f}"
    if typ == "turn_complete":
        u = str(ev.get("utterance", ""))[:40]
        return f"{t}  turn  \"{u}\""
    rest = {k: v for k, v in ev.items() if k not in ("ts", "type")}
    return f"{t}  {typ}  {json.dumps(rest, ensure_ascii=False)[:60] if rest else ''}"


def main(scr) -> None:
    curses.curs_set(0)
    curses.use_default_colors()
    curses.start_color()
    curses.init_pair(1, curses.COLOR_GREEN, -1)
    curses.init_pair(2, curses.COLOR_YELLOW, -1)
    curses.init_pair(3, curses.COLOR_RED, -1)
    curses.init_pair(4, curses.COLOR_CYAN, -1)
    GREEN, YELLOW, RED, CYAN = (curses.color_pair(i) for i in (1, 2, 3, 4))
    scr.nodelay(True)

    st = State()
    while True:
        st.poll()
        scr.erase()
        h, w = scr.getmaxyx()
        gw = max(20, w - 24)  # graph width

        # ---- header ------------------------------------------------------
        alive = st.alive
        status = ("● LIVE", GREEN) if alive else ("✕ DAEMON NOT RUNNING", RED)
        scr.addstr(0, 1, "HERMIT sudo-console", curses.A_BOLD)
        scr.addstr(0, 22, status[0], status[1] | curses.A_BOLD)
        scr.addstr(0, max(46, w - 34), time.strftime("%H:%M:%S"))
        scr.addstr(0, max(56, w - 24), f"state: {STATE_DIR}")

        # ---- mute switches ----------------------------------------------
        mic = ("MIC MUTED ", RED | curses.A_BOLD) if st.mic_muted else ("mic live  ", GREEN)
        spk = ("SPKR MUTED", RED | curses.A_BOLD) if st.speaker_muted else ("spkr live ", GREEN)
        scr.addstr(2, 1, "[m] ")
        scr.addstr(2, 5, *mic)
        scr.addstr(2, 18, "[s] ")
        scr.addstr(2, 22, *spk)
        if st.live.get("listening"):
            scr.addstr(2, 36, "◉ STT STREAMING", CYAN | curses.A_BOLD)

        # ---- mic level ---------------------------------------------------
        rms = st.rms_hist[-1] if st.rms_hist else -99.0
        scr.addstr(4, 1, "mic dBFS", curses.A_BOLD)
        scr.addstr(4, 11, f"{rms:6.1f} ")
        scr.addstr(4, 19, bar(rms, -60.0, 0.0, gw)[: max(0, w - 20)],
                   GREEN if rms < -12 else YELLOW if rms < -4 else RED)
        scr.addstr(5, 19, spark(st.rms_hist, -60.0, 0.0, min(gw, w - 20)))

        # ---- wake score --------------------------------------------------
        ww = st.ww_hist[-1] if st.ww_hist else 0.0
        thr = st.live.get("ww_threshold") or 0.5
        flash = (time.time() - st.last_wake) < FLASH_S
        scr.addstr(7, 1, "wake", curses.A_BOLD | (RED if flash else 0))
        scr.addstr(7, 11, f"{ww:6.3f} ")
        colour = RED if flash else GREEN if ww >= thr else YELLOW if ww >= thr / 2 else 0
        scr.addstr(7, 19, bar(ww, 0.0, 1.0, gw)[: max(0, w - 20)], colour)
        scr.addstr(8, 19, spark(st.ww_hist, 0.0, 1.0, min(gw, w - 20)))
        # threshold marker on the bar row
        tx = 19 + round(max(0.0, min(1.0, thr)) * gw)
        if 19 <= tx < w - 1:
            scr.addstr(7, tx, "|", CYAN | curses.A_BOLD)
        scr.addstr(9, 19, f"threshold {thr:.2f}  (| marker)", CYAN)
        if flash:
            scr.addstr(9, 1, "WAKE!", RED | curses.A_BOLD | curses.A_BLINK)

        # ---- events ------------------------------------------------------
        scr.addstr(11, 1, "events", curses.A_BOLD)
        rows = h - 13
        evs = list(st.events)[-rows:]
        for i, ev in enumerate(evs):
            attr = RED | curses.A_BOLD if ev.get("type") == "ww_fired" else 0
            scr.addstr(12 + i, 3, fmt_event(ev)[: w - 4], attr)

        scr.addstr(h - 1, 1, "m mic  s speaker  c clear  q quit"[: w - 2], curses.A_DIM)
        scr.refresh()

        # ---- keys --------------------------------------------------------
        try:
            k = scr.getkey()
        except curses.error:
            k = ""
        if k == "m":
            st.mic_muted = not st.mic_muted
            st.write_control()
        elif k == "s":
            st.speaker_muted = not st.speaker_muted
            st.write_control()
        elif k == "c":
            st.events.clear()
        elif k in ("q", "Q"):
            # Leave nothing muted behind: a console that exits with mutes on
            # would silently deafen the device until someone found control.json.
            st.mic_muted = st.speaker_muted = False
            st.write_control()
            return
        time.sleep(0.12)


if __name__ == "__main__":
    try:
        curses.wrapper(main)
    except KeyboardInterrupt:
        pass
