# HERMIT

A headless, voice-first personal agent for a Raspberry Pi 4 (1GB). One Rust binary,
five tools, four-layer memory, and a self-learning loop. Optimized above all for the
time between "you stop talking" and "it starts answering".

```
 mic strip ──FPC──> XVF3800 core board ──USB(UAC2)──> Pi 4 ──> Cerebras / Parallel / Firecrawl
   (AEC, beamforming, NS,                    ^                        Deepgram / Cartesia
    dereverb, AGC, VAD, DoA                  │
    all in hardware @16kHz)                  └── speaker amp ──> DFRobot FIT0502 (3W, 8Ω)
```

The reSpeaker Flex is the Pi's **only** sound card. Microphone capture and every kind
of playback — speech, Spotify, radio — go through it, so the XVF3800's hardware echo
canceller always has a loopback reference. That is what makes barge-in work: the wake
word is heard while music is playing.

## Latency targets, and what is measured

`scripts/bench.sh` measures all five and exits non-zero if any p50 misses.

| Gate | Target | Measured on the Pi |
| --- | --- | --- |
| Local harness overhead (route + recall + assemble) | ≤ 15 ms | **1.1 ms** ✅ |
| Text: first token, no tools | < 700 ms | **366 ms** ✅ |
| Fast-path device command (pause/volume/next) | < 50 ms, no LLM call | **1.0 ms** ✅ |
| Voice: first audio, no tools | < 1.2 s | needs a TTS key |
| Voice: first audio, one web search | < 2.0 s | needs TTS + search keys |

Measured on a Raspberry Pi 4 over Wi-Fi against live Cerebras. **Connection
pre-warming is worth ~130 ms of that**: raw `curl` with a cold TLS handshake gets first
byte in 497 ms p50, the daemon with a pooled warm connection in 366 ms.

## Build and deploy

Never compile on the Pi. You need Docker (on macOS: `brew install colima docker &&
colima start`).

```bash
cd hermit
scripts/build-pi.sh                 # -> target/aarch64-unknown-linux-gnu/release/hermit
scripts/deploy.sh <pi-host>         # rsync binary + config + firmware to the Pi
```

`build-pi.sh` picks the right path for your host. **On Apple Silicon `cross` does not
work** — its image for this target is x86_64-only and it dies trying to install an
x86_64 toolchain in an arm64 container. Since host and target are both aarch64 there,
the script just runs `cargo build` inside an arm64 `rust:1-bookworm` container: no
emulation, and the same glibc 2.36 as Raspberry Pi OS Bookworm. On x86_64 hosts it
falls back to `cross` per `deploy/Cross.toml`.

The build is always `--features pi`: that enables ALSA, the Porcupine wake word, and
sd_notify. Without it `Type=notify` never sees `READY=1` and systemd kills the unit at
start-up.

Then, per `deploy/README.md`: flash firmware → `provision.sh` → deploy → fill
`/etc/hermit/hermit.env` → start the service. `scripts/phase0.sh` runs on the Pi and
collects every Phase 0 fact into one report.

### Operator console

`sudo-console` is installed on the Pi by `provision.sh`. It is a dependency-free
curses TUI with labelled sections (CONTROLS / AUDIO / CONVERSATION / ACTIVITY)
showing the live microphone waveform/RMS level, wake-word score and threshold,
music volume, wake activations, interim/final user transcripts, and assistant
replies. Keys: `m` mic mute, `s` speaker mute, `-`/`+` volume in 5% steps
(arrow keys work too), `r` restart HERMIT, `b` reboot, `p` power off, and `q`
quit. Lifecycle actions require a second `y` confirmation and pass through a
fixed root-owned helper with a narrowly scoped sudoers rule; the TUI never
receives general root access.

Mutes and volume ride the same `control.json` lease file but with different
semantics, both deliberate: mutes are a **lease** (TTL-expired, so a crashed
console can never leave the device deaf or silent), while volume is a
**command** — sent only after the operator first touches `-`/`+`, applied by
the daemon edge-triggered, and persistent after the console exits. That is
what lets console volume and voice volume ("Sudo, volume up") coexist without
fighting. The daemon publishes its actual volume in `live.json`; the console
gauge shows that truth, marking a not-yet-acknowledged request with `*`.

```bash
cargo test            # 241 tests, no network or API keys needed
cargo run -- check --config config/hermit.toml   # validate config
```

## Layout

```
src/
  config.rs        every tunable; hot-reloaded via notify
  http.rs          one pooled client + connection pre-warming
  router.rs        fast-path device commands (no LLM)
  orchestrator.rs  the turn loop; 2-round tool cap
  llm/             Cerebras SSE streaming + tool-call reassembly
  tools/           the five tools: search, fetch, news, music, research
  memory/          SQLite + FTS5, prompt assembly, the write firewall
  reflect.rs       nudges, skill distillation, nightly consolidation
  speech/          chunker, TTS, STT, wake word, canned acks
  feedback.rs      the self-refining loop: verdicts, bounded tuning, wake clips
  audio/           ring buffer + ALSA, with instant barge-in flush
  music/           mpv IPC + Spotify Web API
  gateway/         CLI, WebSocket, voice pipeline
config/            hermit.toml, prompts/*.md, skills/, stations.toml
deploy/            provision.sh, asound.conf, systemd units, Cross.toml
docs/BRINGUP.md    Phase 0 hardware checklist
docs/XVF3800.md    mic-array control plane: what we tune and why
tools/xvf3800/     xvf.py USB control tool + setup.sh (udev, AGC tuning)
scripts/           bench.sh, flash_notes.md
```

## Design decisions worth knowing

**Two tool rounds, hard.** The final round is offered no tool schemas, *and* any tool
calls it emits anyway are dropped. Without that second half the cap silently becomes
three rounds — there is a regression test for exactly this.

**The memory firewall.** Raw web pages, search excerpts and tool output are never
written to memory. It is structural, not conventional:

- `Store` has no public "insert this text as a fact". The only path is
  `apply_reflection`, which takes a `ReflectionBatch` — and the only thing that builds
  one is `reflect::parse_extraction`, from the reflection model's own JSON.
- `record_message` refuses any role but `user`/`assistant`, so tool output never enters
  the transcript that reflection reads.
- Background research results are recorded as ordinary assistant messages, so they go
  through reflection like anything else rather than taking a shortcut.

`tests/memory_firewall.rs` asserts all of it with a poisoned page.

**Streaming everywhere.** Cerebras tokens → sentence chunker → TTS websocket → ALSA.
The first clause is being spoken while the rest is still generating. The chunker emits
its first chunk at the earliest of {punctuation after ≥12 tokens, 250 ms, 40 tokens}.

**Persistent websockets.** A cold TLS handshake is 100–400 ms against a 1.2 s budget,
so the TTS socket is opened at boot and reused per utterance with a fresh context id.
HTTP upstreams are pre-warmed at boot and re-warmed every 4 minutes.

**No embedding model.** Retrieval is BM25 over FTS5. It costs no RAM, and on a corpus
this size it retrieves at least as well as a small embedding model would.

## Things found while building that differ from the original plan

These were adapted per the spec's "adapt parameter names, never the architecture" rule.
None change the design.

0. **`cross` is unusable on Apple Silicon.** See "Build and deploy" above; the
   `scripts/build-pi.sh` native-container path replaces it there.

1. **The wake word is the project's own "Hey Sudo" model, not Porcupine.** The
   trained classifier (`hey_sudo.onnx`, an openWakeWord-style livekit-wakeword
   model) is ported to Rust in `src/speech/wake_onnx.rs`: three ONNX graphs per
   2-second window (melspectrogram → embeddings → classifier), verified
   numerically against the Python reference (0.747 vs 0.753 on the same clip;
   the delta is 80 ms window alignment, not maths). onnxruntime is `dlopen`'d —
   a missing `libonnxruntime.so` degrades to "wake word disabled" instead of a
   binary that will not boot. Two gotchas that cost real time: the classifier
   needs a full 2.0 s window (shorter clips score 0.0 forever), and scoring
   must be strided (`SCORE_STRIDE_FRAMES=4`) — per-frame scoring runs 2.5×
   behind real time on the Pi and the resulting ALSA overruns corrupt all
   downstream audio. Porcupine survives only as an optional fallback engine in
   `src/speech/wake.rs`, bound through its five-function C ABI.

2. **Cartesia's `max_buffer_delay_ms` defaults to 3000 ms.** That one field would blow
   the 1.2 s first-audio gate on its own. It is pinned to 0, and `Config::validate`
   refuses to start with any other value.

3. **Parallel Search uses `x-api-key`, not bearer auth**, and the body field is
   `search_queries` (an array). The spec's central warning is confirmed by their docs:
   omitting `mode` defaults to `advanced` at ~3 s versus turbo's ~200 ms. Validation
   refuses to start unless `search.mode == "turbo"`.

4. **Firecrawl scrape is `/v2/scrape`.**

5. **Fact deduplication uses token containment, not a BM25 threshold.** BM25 scores
   depend on corpus size, so a fixed cutoff dedupes nothing on a fresh device and
   everything on a mature one. Containment is corpus-independent and reads as a plain
   fraction (`memory.dedupe_similarity`, default 0.8).

## Hardware findings (measured, not assumed)

The reSpeaker Flex XVF3800 accepts **16 kHz and no other rate**, in both directions.
Since TTS is requested at 16 kHz too, nothing in the chain ever resamples.

**Its capture clock is slaved to the playback stream.** Capture alone returns `EIO` with
zero frames; capture while playback streams works; capture the moment playback stops
fails again. A voice assistant that only opens playback when it wants to speak would
have a permanently deaf microphone. `AudioPlayer::spawn_keepalive` therefore writes
silence whenever the play queue is empty (`audio.keepalive_silence`, default on). It
also keeps the AEC reference continuously fed.

**Hardware AEC confirmed.** Recording both channels with silence, then with a 440 Hz
tone playing through the speaker:

| channel | silence | tone playing | Δ |
|---|---|---|---|
| ch0 (processed) | −50.4 dBFS | −57.2 dBFS | **−6.8 dB** |
| ch1 (reference) | −43.1 dBFS | −27.7 dBFS | **+15.4 dB** |

~22 dB relative suppression — the echo is cancelled, and ch0 is the correct channel for
the daemon.

The board already shipped USB firmware, so no flash was needed. The image is not
redistributed in this repo; `firmware/fetch.sh` downloads it from Seeed's repository
and verifies it against the pinned checksum. Its DFU id is `2886:801c` (the wiki says `001a`, which
is the *normal* mode id here).

Two more field notes that cost real time:

- **Under-voltage looks like everything else.** The Pi originally brown-out
  cycled (11 `Undervoltage detected!` events in 4 minutes), crashing under
  load, killing the network, and interrupting a dist-upgrade mid-transaction.
  `vcgencmd get_throttled` must read `0x0`; anything else, fix power first —
  the official 5.1 V/3 A PSU — before debugging software.
- **Never run `speaker-test` without a bound.** It loops forever; an orphaned
  one kept beeping at the operator long after the SSH session died. Always
  `timeout N speaker-test ... -l 1`, and `pkill -9 speaker-test` if in doubt.

## Verification status

**Verified here** — 241 tests pass on the dev machine with no network and no API keys:
the chunker, router, memory and firewall, tool schemas and dispatch, SSE decoding and
streaming tool-call reassembly, the 2-round cap, mpv IPC (against a fake mpv speaking
the real dialect), audio mixing and barge-in generation logic, and the Cartesia/
Deepgram/Parallel/Firecrawl request shapes.

**Verified by the aarch64 build** — `scripts/build-pi.sh` compiles the full
`--features pi` graph for the Pi, including `src/audio/alsa_sink.rs` against real ALSA
headers, the Porcupine dlopen binding, and sd_notify. That build caught two bugs that
existed only in feature-gated code (a missing `Path` import in the Porcupine module,
and `sd-notify` 0.5 having dropped an argument from `notify()`); both are fixed.

**Not verified here** — anything that needs the hardware or a paid key:

- Real TTFT, first-audio and barge-in numbers. Run `scripts/bench.sh` on the Pi.
- The Porcupine C ABI call, against the real `libpv_porcupine.so`.
- Live Cartesia / Deepgram / Parallel / Firecrawl / Spotify responses.

Start with `docs/BRINGUP.md`; every later phase depends on the sample rate it tells you
to record.

## Honest limitations

- **Spotify Connect on new accounts is broken by Spotify, not by this project.**
  Accounts on Spotify's post-2024 DRM path get every legacy audio-key request
  refused (`error audio key 0 1`), on Premium too, across librespot, spotifyd
  and go-librespot alike; only official clients with licensed DRM play. Search,
  Connect registration and device control all still work. When playback cannot
  produce audio the daemon detects the stall (progress, not status flags) and
  falls back to YouTube via mpv + yt-dlp, labelled honestly "(via YouTube)".
  Spotify Connect control still requires Premium.
- **Apple Music cannot be driven by the agent.** AirPlay (`shairport-sync`, optional
  and off by default) lets you *push* audio to the device from an Apple device, but
  the agent cannot control playback. Nothing on Linux can.
- **This is not hi-fi.** A 16 kHz voice pipeline into a 3W mono driver. Fine for
  speech, news and casual music. Do not "fix" it by changing the audio topology — the
  single-card path is what gives you hardware AEC and barge-in.
- **Turbo search covers English and Japanese.** Script detection routes other writing
  systems to `basic`, but it cannot tell English from Spanish, so a Spanish query still
  goes to turbo. Fixing that needs a language-ID model, which the RAM budget rules out.
- **Live radio has no "next track".** The device says so rather than silently ignoring
  you.
