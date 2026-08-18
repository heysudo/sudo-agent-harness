#!/usr/bin/env python3
"""Generate earcon WAVs (mono s16le 16k). All <150ms except ready chirp.

- trigger_ack: quick two-tone up chirp (played instantly on trigger)
- session_close: soft down chirp
- attention: distinct chime for proactive speech (Phase 3, generated now)
- ready: boot-complete chirp
"""

import math
import struct
import wave
from pathlib import Path

SR = 16000
OUT = Path(__file__).resolve().parent.parent / "assets" / "earcons"


def tone(freq: float, ms: float, amp: float = 0.35, fade_ms: float = 10) -> list[int]:
    n = int(SR * ms / 1000)
    fade = int(SR * fade_ms / 1000)
    out = []
    for i in range(n):
        env = min(1.0, i / fade, (n - 1 - i) / fade)
        out.append(int(32767 * amp * env * math.sin(2 * math.pi * freq * i / SR)))
    return out


def silence(ms: float) -> list[int]:
    return [0] * int(SR * ms / 1000)


def write(name: str, samples: list[int]) -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    with wave.open(str(OUT / f"{name}.wav"), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(SR)
        w.writeframes(struct.pack(f"<{len(samples)}h", *samples))
    print(f"{name}.wav  {len(samples)/SR*1000:.0f}ms")


write("trigger_ack", tone(880, 60) + tone(1320, 70))
write("session_close", tone(1100, 60) + tone(740, 80, amp=0.3))
write("attention", tone(660, 90, amp=0.3) + silence(40) + tone(660, 90, amp=0.25))
write("ready", tone(523, 90) + tone(659, 90) + tone(784, 120))
# 2026-07-23 (user request): audible state transitions around a turn
write("thinking", tone(600, 50, amp=0.3) + silence(30) + tone(450, 60, amp=0.28))
write("listening", tone(700, 50, amp=0.3) + tone(1050, 60, amp=0.3))
