#!/usr/bin/env python3
import importlib.util
import json
import tempfile
import unittest
from pathlib import Path
from unittest import mock

MODULE_PATH = Path(__file__).with_name("sudo-console.py")
spec = importlib.util.spec_from_file_location("sudo_console", MODULE_PATH)
assert spec is not None and spec.loader is not None
console = importlib.util.module_from_spec(spec)
spec.loader.exec_module(console)


class ConsoleTests(unittest.TestCase):
    def test_formats_conversation_and_wake_events(self):
        user = console.fmt_event({"ts": 0, "type": "transcript", "role": "user", "text": "turn on the lights"})
        assistant = console.fmt_event({"ts": 0, "type": "transcript", "role": "assistant", "text": "Done."})
        wake = console.fmt_event({"ts": 0, "type": "ww_fired", "score": 0.91})
        self.assertIn("YOU", user)
        self.assertIn("turn on the lights", user)
        self.assertIn("SUDO", assistant)
        self.assertIn("Done.", assistant)
        self.assertIn("WAKE", wake)

    def test_malformed_telemetry_is_rendered_without_crashing(self):
        line = console.fmt_event({"ts": "bad", "type": "ww_fired", "score": None})
        self.assertIn("WAKE", line)
        self.assertEqual(console.as_float({"not": "numeric"}, -7.0), -7.0)

    def test_state_loads_existing_control_and_writes_atomically(self):
        with tempfile.TemporaryDirectory() as td:
            state_dir = Path(td)
            (state_dir / "control.json").write_text(
                json.dumps(
                    {"ts": console.time.time(), "mic_muted": True, "speaker_muted": False}
                )
            )
            with mock.patch.multiple(
                console,
                STATE_DIR=state_dir,
                LIVE=state_dir / "live.json",
                EVENTS=state_dir / "events.jsonl",
                CONTROL=state_dir / "control.json",
            ):
                state = console.State()
                self.assertTrue(state.mic_muted)
                state.speaker_muted = True
                state.write_control()
                written = json.loads((state_dir / "control.json").read_text())
                self.assertIn("ts", written)
                self.assertTrue(written["mic_muted"])
                self.assertTrue(written["speaker_muted"])
                self.assertFalse((state_dir / "control.json.tmp").exists())

    def test_stale_control_file_does_not_leave_audio_muted(self):
        with tempfile.TemporaryDirectory() as td:
            state_dir = Path(td)
            (state_dir / "control.json").write_text(
                json.dumps({"ts": 1, "mic_muted": True, "speaker_muted": True})
            )
            with mock.patch.multiple(
                console,
                STATE_DIR=state_dir,
                LIVE=state_dir / "live.json",
                EVENTS=state_dir / "events.jsonl",
                CONTROL=state_dir / "control.json",
            ):
                state = console.State()
                self.assertFalse(state.mic_muted)
                self.assertFalse(state.speaker_muted)

    def test_privileged_actions_use_only_the_fixed_helper(self):
        for action in ("service-restart", "reboot", "poweroff"):
            with mock.patch.object(console.subprocess, "run") as run:
                run.return_value.returncode = 0
                ok, _ = console.run_action(action)
                self.assertTrue(ok)
                run.assert_called_once_with(
                    ["sudo", "-n", console.POWER_HELPER, action],
                    capture_output=True,
                    text=True,
                    timeout=10,
                    check=False,
                )
        ok, message = console.run_action("not-allowed")
        self.assertFalse(ok)
        self.assertIn("not allowed", message)

    def test_spark_and_bar_are_width_bounded(self):
        self.assertEqual(len(console.bar(-30, -60, 0, 12)), 12)
        self.assertEqual(len(console.spark([-60, -30, 0], -60, 0, 12)), 12)

    def test_volume_nudges_from_live_value_and_is_clamped(self):
        state = console.State.__new__(console.State)
        state.volume = None
        state.live = {"volume": 70}
        self.assertEqual(state.nudge_volume(-5), 65)
        self.assertEqual(state.nudge_volume(-5), 60)
        state.volume = 98
        self.assertEqual(state.nudge_volume(+5), 100)
        state.volume = 3
        self.assertEqual(state.nudge_volume(-5), 0)


if __name__ == "__main__":
    unittest.main()
