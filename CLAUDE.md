# CLAUDE.md — HERMIT working notes

Read this before touching the project. It records what was built, what was measured on
real hardware, and the traps that cost time. Everything here was verified on
2026-08-18 unless stated otherwise.

HERMIT is an ultra-low-latency voice agent for a Raspberry Pi 4 (1GB): one Rust
daemon, five tools, four-layer memory with a self-learning loop. The full
specification lives in the conversation that produced it; `hermit/README.md` is the
user-facing doc. This file is the engineering log.

---

## Current status

| Phase | State |
|---|---|
| 0 — hardware bring-up | **PASSED** (enumeration, rates, playback, capture, AEC) |
| 1 — skeleton + streaming | **PASSED on device** (TTFT 366 ms p50, gate 700) |
| 2 — tools | Code complete; verified against real gpt-oss-120b. Needs `PARALLEL_API_KEY` / `FIRECRAWL_API_KEY` for live gates |
| 3 — voice out | **PASSED on device** — first audio 654 / 875 ms (gate 1200) |
| 4 — voice in | **PASSED on device** via `/listen` — live speech transcribed, answered, spoken; first audio 983 ms. Wake word = **"Hey Sudo"**, ported and verified against the Python reference; live hands-free trigger not yet caught on tape |
| 5 — music | Code complete; needs Spotify Premium creds; mpv/librespot sidecars not yet installed |
| 6 — memory | **PASSED** — recall 0.7 ms, cross-session verified with real model |
| 7 — learning loop | **PASSED** — facts extracted, stored, recalled in a fresh session |
| 8 — hardening | Not started (watchdog wired, 24 h soak not run) |

195 tests pass with no network and no API keys. `cargo clippy --all-targets` is clean.

---

## The target device

`<pi-host>` @ `<redacted-lan-ip>`, user `<pi-host>`, SSH key `~/.ssh/<pi-key>`
(config entry `<pi-host>`). **Debian 13 (trixie)**, not Bookworm — the deploy docs
originally assumed Bookworm. The binary is built against Bookworm's glibc 2.36 and runs
fine on trixie's 2.41 (forward compatibility); do not "fix" this by building on trixie
unless the Pi is upgraded past the build image.

Host key fingerprint `<host-key-fingerprint>` — the user has
several Pis on this LAN, and a MAC-OUI scan alone found the wrong one. Verify before
assuming an IP is this device.

Currently on **Wi-Fi**, not Ethernet. The spec prefers Ethernet; Wi-Fi jitter is baked
into every latency figure recorded so far.

`sudo` requires a password on this box (no passwordless sudo).

---

## Hardware facts measured on the board

The reSpeaker Flex XVF3800 enumerates as:

```
2886:001a  Seeed Technology Co., Ltd. reSpeaker XVF3800 4-Mic Array   (normal)
2886:801c  reSpeaker XVF3800 Safe Mode                                (DFU)
```

Note `801c` for Safe Mode — the Seeed wiki quotes `2886:001a` for DFU, which is wrong
for this board. Trust `lsusb`.

**It already carries USB firmware — no flash was needed.** A verified 2-channel/16 kHz
linear image is checked in at `hermit/firmware/` with `SHA256SUMS` if it is ever
required. The wiki's filenames (`respeaker_flex_ua-io16-lin.bin`) do not exist; the
real repo is <https://github.com/respeaker/reSpeaker_Flex> under `xmos_firmwares/usb/`
using the scheme `respeaker_flex_usb_<l|c><rate>k<n>ch_v<ver>.bin`.

Hardware parameters, from `--dump-hw-params`:

```
Playback: S16_LE, CHANNELS 2, RATE 16000   <- the ONLY rate offered
Capture : S16_LE, CHANNELS 2, RATE 16000   <- the ONLY rate offered
```

16 kHz and nothing else, both directions. Since TTS is also requested at 16 kHz,
**nothing in the chain resamples**. `asound.conf` is set to `card "Array"`,
`rate 16000`, period 160 / buffer 960.

After `provision.sh` blacklists onboard audio, the Flex is **card 0** and the only card.
Before that it moved between index 1 and 2 across reboots — which is exactly why
`asound.conf` refers to it by *name*, never index. Keep it that way.

### THE BIG ONE: capture is slaved to playback

The XVF3800's USB capture endpoint is SYNC type and its capture clock only runs while a
playback stream is active. Measured:

| condition | result |
|---|---|
| capture alone | `EIO`, zero frames |
| capture while playback streams | works, full-length audio |
| capture after playback stops | `EIO` again, immediately |

**A voice assistant that only opens playback when it wants to speak has a permanently
deaf microphone.** This cost an hour and was initially misattributed to a power fault.

The fix is `AudioPlayer::spawn_keepalive` in `src/audio/mod.rs`: when the play queue is
empty the audio thread writes silence instead of blocking, so the device is never idle.
Controlled by `audio.keepalive_silence` (default true). **Do not disable it.** Two
regression tests guard this. Bonus: it keeps the AEC reference path continuously fed.

### AEC is confirmed working

Controlled test — record both channels with silence playing, then with a 440 Hz tone:

| channel | silence | tone | Δ |
|---|---|---|---|
| ch0 (processed) | −50.4 dBFS | −57.2 dBFS | **−6.8 dB** |
| ch1 (reference) | −43.1 dBFS | −27.7 dBFS | **+15.4 dB** |

~22 dB relative suppression. ch0 is the processed voice channel the daemon must use;
`hermit_in` maps to it correctly.

### Power

The Pi originally brown-out cycled continuously (`Undervoltage detected!` /
`Voltage normalised`, 11 events in 4 minutes, at 41 °C). It crashed under load, killed
the network three times, and left dpkg mid-transaction. **Now fixed** — `throttled=0x0`.
The user also feeds the Flex supplementary power via the XIAO USB-C port.

If under-voltage returns: the Pi needs the official 5.1 V/3 A PSU. Note the brownouts
were present at first boot with **no USB device attached**, so board power is not the
cause. Repeated brownouts during SD writes risk filesystem corruption — one interrupted
dist-upgrade already happened, repaired with `dpkg --configure -a`.

---

## Build and deploy

```bash
cd hermit
scripts/build-pi.sh              # aarch64 binary, --features pi
scripts/deploy.sh <pi-host>     # rsync binary + config + firmware, install if provisioned
scripts/deploy.sh <pi-host> --restart
```

**`cross` does NOT work on Apple Silicon.** Its image for aarch64-unknown-linux-gnu is
x86_64-only, so on an arm64 host it tries to install an x86_64 Rust toolchain inside an
arm64 container and dies. `build-pi.sh` detects this and instead runs a plain
`cargo build` inside an arm64 `rust:1-bookworm` container — no emulation, same glibc as
Pi OS Bookworm. On x86_64 hosts it falls back to `cross` per `deploy/Cross.toml`.

Container runtime is colima (`colima start`). A stale `credsStore: desktop` in
`~/.docker/config.json` blocked image pulls; removed (backup `.bak-hermit`).

macOS ships **openrsync**, which rejects `--info` and `--no-owner`. `deploy.sh` probes
for this and falls back to a portable flag set.

Always build with `--features pi`. Without it there is no ALSA, no wake word, and no
sd_notify — so `Type=notify` never sees `READY=1` and systemd kills the unit at the
start timeout. Two bugs existed *only* in that feature-gated code and were invisible to
`cargo test` on the dev machine (a missing `Path` import; `sd-notify` 0.5 dropping an
argument from `notify()`). **Run the Pi build before claiming anything compiles.**

---

## Verified behaviour worth not re-deriving

- **Connection pre-warming earns ~130 ms.** Raw `curl` to Cerebras from the Pi with a
  cold TLS handshake: 497 ms p50. The daemon with a pooled pre-warmed connection:
  366 ms. This is spec §5 paying for itself, measured.
- **Real `gpt-oss-120b` tool calls reassemble correctly.** The model emits fragmented
  tool-call deltas across SSE frames; `ToolCallAccumulator` rebuilds them keyed on
  `index` (the only field present on every fragment). Verified against the live model,
  not just the stub.
- **The learning loop works end to end.** Four conversational turns → reflection nudge →
  4 facts stored with sensible tags and importance (0.9 for standing instructions and
  location, 0.6 for preferences) → a *fresh session* answered "what is my dog's name?"
  from memory alone. Recall 0.7 ms.
- **The memory firewall holds.** `select count(*) from messages where role not in
  ('user','assistant')` returns 0.
- gpt-oss emits hidden `reasoning` deltas even at `reasoning_effort=low`. They are
  discarded and never spoken; TTFT is measured on the first *content* token.

---

## The wake word is "Hey Sudo" — NOT Porcupine

The project already trained its own wake word. It lives in `heysudo/sudo` at
`sudoedge/models/hey_sudo.onnx` and is a **livekit-wakeword** (openWakeWord-style)
classifier. Do not go looking for a Picovoice key — it is not needed and the
Porcupine path is only a fallback.

`src/speech/wake_onnx.rs` is a faithful Rust port of `livekit.wakeword`'s
`WakeWordModel.predict`. Three ONNX graphs run in sequence per 2.0 s window:

```
2.0 s int16 (25 x 80 ms frames)
  -> /32768 to f32
  -> melspectrogram.onnx    (1, samples) -> (time, 32) dB mel, then x/10 + 2
  -> embedding_model.onnx   76-mel windows @ stride 8 -> (96,) each
  -> last 16 embeddings -> (1, 16, 96)
  -> hey_sudo.onnx          -> score, fire at >= 0.5
```

**Port verified numerically against the Python reference on identical audio:**

| clip | Python | Rust |
|---|---|---|
| TTS "hey sudo" (padded to >2 s) | 0.753 FIRES | **0.747 WAKE @ 2.16 s** |
| TTS "hello there, how are you today" | 0.007 no | **0 detections** |

The small delta is window alignment (Rust steps on 80 ms frame boundaries), not a
maths difference.

Gotchas:
- **The classifier needs a full 2.0 s window.** A 1.3 s clip scores 0.0 forever. Pad
  short test clips with silence or the result is meaningless.
- `wake.sensitivity` is the **detection threshold** (0..1) for this engine, not a
  Porcupine sensitivity. 0.5 is the reference default.
- The two upstream graphs must stay the exact versions the classifier was trained
  against. `models/SHA256SUMS` pins all three.
- onnxruntime is `dlopen`'d (`ort` `load-dynamic`). `libonnxruntime.so` 1.29.0 for
  aarch64 is installed to `/usr/local/lib` on the Pi; set `ORT_DYLIB_PATH` if it
  moves. Missing library => wake word disabled, everything else still runs.
- `hermit wake-score <file.wav>` scores a recording offline — use it to tune the
  threshold from real room audio instead of guessing.
- The detector logs a heartbeat (`hey-sudo listening windows=N max_score=X`) every
  ~4 s. **A silently starved detector looks exactly like a quiet room**; the
  heartbeat is how you tell them apart. Confirmed live at `max_score=0.098` on
  ordinary room speech.

## Bugs found and fixed on hardware in the voice phases (do not reintroduce)

1. **Keepalive underrun storm.** The first keepalive implementation slept when a
   write returned "fast". A blocking ALSA write returns when frames are *accepted*,
   not played, so on an empty buffer it returns instantly; sleeping on that starves
   the buffer and produced an `EPIPE` every ~20 ms. Fix: never sleep — write straight
   back and let ALSA block. Backends that don't block (null/test) pace themselves in
   `write()`. Result: 0 underruns.
2. **Utterances truncated after exactly 32 frames.** `forward_until_done` treated a
   `TrySendError::Full` on the utterance channel as "STT finished" and hung up.
   During the ~1 s Deepgram handshake the channel fills, so every utterance ended
   after 32 chunks (the channel depth) with `close=1000` and an empty transcript.
   Fix: drop the chunk on `Full`, break only on `Closed`; channel widened to 128.
3. **Silent Deepgram close frames.** The client discarded the close code, so a
   rejected connection was indistinguishable from silence. Now logged.

## Known issues / open items

1. **`capture_ref` PCM returns silence.** The debug-only ch1 tap in `asound.conf` opens
   fine but yields no audio, while raw ch1 clearly carries signal (−43 dBFS). Tried both
   `ttable.1.0` and `ttable.0.1`; neither worked, and an attempt to A/B the ttable index
   order failed to load the test PCMs, so the convention is still unresolved. **Does not
   affect runtime** — the daemon uses `hermit_in` (ch0), which works and is AEC-verified.
   Fix by testing ttable orders properly, or switch to `dsnoop` `bindings.<client> <slave>`
   which has unambiguous semantics.
2. **Sidecars not installed.** `librespot` is not in the Debian archive; `provision.sh`
   prints a manual install path rather than piping a binary to root. `mpv` likewise not
   yet confirmed installed. Phase 5 needs both.
3. **Never run `speaker-test` without a bound.** It loops forever. An orphaned
   `speaker-test` kept beeping at the user after an SSH timeout. Always use
   `timeout N speaker-test ... -l 1`, and `pkill -9 speaker-test` if in doubt.
4. **Wi-Fi, not Ethernet.** Latency figures carry Wi-Fi jitter (ping 7–17 ms, previously
   17–113 ms when power was bad).
5. **Missing keys**: Parallel, Firecrawl (live tool gates), Spotify. Cartesia +
   Deepgram are installed on the Pi. Cartesia voice: Skylar
   `db6b0ed5-d5d3-463d-ae85-518a07d3c2b4`. Picovoice is NOT required — see the
   "Hey Sudo" section.
6. **Live hands-free wake not yet demonstrated.** Every attempt so far captured an
   empty room or a TV (Deepgram transcribed a documentary), never the phrase. The
   detector heartbeat proves it is scoring; it simply has not heard "hey sudo" yet.
6. **24 h thermal soak (Phase 8) not run.**
7. `deploy/provision.sh` was written assuming Bookworm; it ran fine on trixie but its
   librespot notes reference the Bookworm archive.

---

## Rules that are LOCKED — do not "improve" these

- One sound card. All playback and capture go through the XVF3800 so hardware AEC always
  has a loopback reference. No Pi 3.5 mm jack, no extra DAC/amp, no ESP32 Wi-Fi satellite.
- Exactly five tools. No more.
- Two interactive tool rounds, hard. The final round is offered no tools *and* any tool
  calls it emits anyway are dropped — without that second half the cap silently becomes
  three. There is a regression test.
- Memory writes only through the reflection channel. `Store` has no public "insert this
  text as a fact"; `record_message` refuses any role but user/assistant.
- `search.mode` must be `"turbo"` — the Parallel API defaults to `advanced` (~3 s vs
  ~200 ms) when unset. `Config::validate` refuses to start otherwise.
- `tts.cartesia_max_buffer_delay_ms` must be 0 — the provider default is 3000 ms, which
  alone exceeds the 1.2 s first-audio budget.
- No Python/Node in the hot path, no PipeWire/PulseAudio, no Docker on the Pi, no
  compiling on the Pi, no local models.

---

## Handy commands

```bash
ssh <pi-host>
ssh <pi-host> 'vcgencmd get_throttled; vcgencmd measure_temp'   # 0x0 = healthy
ssh <pi-host> 'cat /proc/asound/cards; aplay -L | head'
ssh <pi-host> 'sudo systemctl status hermit; journalctl -u hermit -n 50'
ssh <pi-host> 'pkill -9 speaker-test aplay arecord'             # stop stuck audio

cd hermit && cargo test && cargo clippy --all-targets
cargo run -- check --config config/hermit.toml
scripts/bench.sh --bin ./target/release/hermit --config <cfg> --runs 20
```

Secrets live in `/etc/hermit/hermit.env` on the Pi (0600). Never in the repo.
