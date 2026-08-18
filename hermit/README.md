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

## Latency targets

| Gate | Target |
| --- | --- |
| Local harness overhead (route + recall + assemble) | ≤ 15 ms |
| Text: first token, no tools | < 700 ms |
| Voice: first audio, no tools | < 1.2 s |
| Voice: first audio, one web search | < 2.0 s |
| Fast-path device command (pause/volume/next) | < 50 ms, no LLM call |

`scripts/bench.sh` measures all five and exits non-zero if any p50 misses.

## Build and deploy

Never compile on the Pi.

```bash
cargo install cross --git https://github.com/cross-rs/cross
cd hermit
export PKG_CONFIG_ALLOW_CROSS=1
cross build --release --target aarch64-unknown-linux-gnu --features pi
```

`--features pi` is required: it enables ALSA, the Porcupine wake word, and sd_notify.
Without it `Type=notify` never sees `READY=1` and systemd kills the unit at start-up.

Then, per `deploy/README.md`: flash firmware → `provision.sh` → rsync binary and
config → fill `/etc/hermit/hermit.env` → start the service.

```bash
cargo test            # 192 tests, no network or API keys needed
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
  audio/           ring buffer + ALSA, with instant barge-in flush
  music/           mpv IPC + Spotify Web API
  gateway/         CLI, WebSocket, voice pipeline
config/            hermit.toml, prompts/*.md, skills/, stations.toml
deploy/            provision.sh, asound.conf, systemd units, Cross.toml
docs/BRINGUP.md    Phase 0 hardware checklist
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

1. **Porcupine has no Rust binding any more.** `pv_porcupine` on crates.io is an empty
   0.0.0 placeholder, and `Picovoice/porcupine` no longer ships `binding/rust` at all.
   Porcupine is still the engine; it is bound through its stable five-function C ABI,
   loaded at runtime with `dlopen`. A missing `.so` degrades to "wake word disabled"
   instead of a binary that will not boot. See `src/speech/wake.rs`.

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

## Verification status

**Verified here** — 192 tests pass on the dev machine with no network and no API keys:
the chunker, router, memory and firewall, tool schemas and dispatch, SSE decoding and
streaming tool-call reassembly, the 2-round cap, mpv IPC (against a fake mpv speaking
the real dialect), audio mixing and barge-in generation logic, and the Cartesia/
Deepgram/Parallel/Firecrawl request shapes.

**Not verified here** — anything that needs the hardware or a paid key:

- `src/audio/alsa_sink.rs` is the one module that cannot even be compile-checked on a
  non-Linux dev machine (`alsa-sys` needs `libasound` + pkg-config for the target). It
  is deliberately thin — all routing, downmixing and the speaker-protection ceiling
  live in `deploy/asound.conf` — and is first compiled by `cross build --features pi`.
- Real TTFT, first-audio and barge-in numbers. Run `scripts/bench.sh` on the Pi.
- The Porcupine C ABI call, against the real `libpv_porcupine.so`.
- Live Cartesia / Deepgram / Parallel / Firecrawl / Spotify responses.

Start with `docs/BRINGUP.md`; every later phase depends on the sample rate it tells you
to record.

## Honest limitations

- **Spotify Connect control requires Premium.** There is no way around this.
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
