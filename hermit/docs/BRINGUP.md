# HERMIT — Phase 0 hardware bring-up

This is the checklist you work through with the hardware on the bench, before
any of the daemon exists on the device. Every step has one command to run and
one explicit pass/fail criterion. Do them in order; later steps assume earlier
ones passed.

**Fill in the results table at the bottom as you go.** Later phases genuinely
depend on those numbers — the native sample rate decides what PCM format we ask
the TTS provider for, and the measured time-to-first-token decides how much
latency budget the rest of the pipeline has. Re-measuring later means taking the
enclosure apart.

---

## The topology, so nothing here is ambiguous

The reSpeaker Flex XVF3800 in USB mode is the Pi's **one and only** sound card.
Microphone capture and **all** playback — TTS, Spotify via librespot, internet
radio via mpv — go through it. That is what keeps the XVF3800's hardware AEC
supplied with a valid loopback reference, and that is what makes barge-in work:
the wake word is still heard while music or TTS is playing.

The Pi's 3.5 mm jack is not used. There is no second DAC and no second amp. The
XIAO ESP32S3 is idle and is not a Wi-Fi audio satellite.

Fidelity, plainly: this is a **16 kHz voice-grade pipeline into a 3 W mono
driver**. It is fine for speech, news and casual music. It is not hi-fi, it is
not meant to be, and it is not to be "fixed" by changing any of the above.

---

## Bill of materials for this phase

| Item | Note |
|---|---|
| Raspberry Pi 4 Model B, 1 GB | Raspberry Pi OS Lite **64-bit**, headless |
| Official 5.1 V / 3 A PSU | not a phone charger — see step 6 |
| Ethernet cable | preferred over Wi-Fi for latency stability |
| reSpeaker Flex XVF3800 Linear-4, "with XIAO ESP32S3" SKU | mic strip + core board over FPC |
| DFRobot FIT0502 speaker | passive, 3 W, 8 Ω, JST PH2.0 |
| USB data cable | a charge-only cable will waste an hour of your life |
| Heatsink, and a 25 mm fan if the enclosure is tight | see step 6 |
| Foam / grommets for decoupling the speaker | see step 0 |

---

## Step 0 — Mechanical and power

Do this before flashing, because it changes what the acoustic tests in step 4
actually measure.

**Mic strip.** Mount at the **front edge**, port slots facing the talker. The
strip is 110 mm long with 33 mm microphone spacing; the ports are on the
bottom/back face, so do not press that face flat against a solid panel — leave
an air gap or cut matching slots.

**Speaker.** Mount at the **far end** from the mic strip, and **mechanically
decouple it** with foam or rubber grommets. Cone vibration conducted through
the chassis into the mic PCB is structure-borne, not airborne — the XVF3800's
AEC cannot cancel it, and it will show up as an unremovable noise floor when
music plays. This is the single most common way to make step 4 fail for
reasons that look like a firmware problem.

**Speaker wiring.** FIT0502 → the JST amplifier output on the Flex **core
board**. Do not wire it to the 3.5 mm aux out and do not use the Pi's jack.

**Enclosure.** Vents, plus a heatsink on the SoC. If the enclosure is tight,
add a 25 mm fan. Sustained thermal throttling starts around **80 °C**.

**Power.** Official 5.1 V / 3 A PSU into the Pi. The Pi's total downstream USB
budget is about **1.2 A**, shared. Start with the Flex on bus power; if you get
audible clicks or brownouts at volume, feed the Flex from its **separate 5 V
JST / 12 V terminal input** instead. Expect to need this once the speaker is
driven hard in a finished enclosure.

- **PASS:** assembled, speaker decoupled, mic ports facing out and unobstructed,
  Pi on the official PSU, Ethernet connected, `ssh` works.
- **FAIL:** speaker rigidly coupled to the same panel as the mic strip → redo it
  now, not after step 4 fails.

---

## Step 1 — Flash the USB 2-channel firmware and confirm enumeration

The "with XIAO ESP32S3" SKU ships with **I2S firmware** and is **not visible as
a USB audio device out of the box**. This is not optional and it is not a
troubleshooting step — it is the first thing that must happen.

Full procedure: **`hermit/scripts/flash_notes.md`**. Summary:

```bash
sudo apt-get install -y dfu-util usbutils

# Power off completely. Hold BOOT. Reconnect power. Hold ~2s. Release.
# (Confirm the button against the Seeed wiki — revisions differ.)

dfu-util -l                       # must list a DFU interface, alt=1 = upgrade
sudo dfu-util -R -e -a 1 -D ~/respeaker-fw/respeaker_flex_usb_l16k2ch_v1.0.3.bin
```

The **2-channel linear** variant is required: channel 0 is the processed mono
voice, channel 1 is the AEC reference. The 1-channel variant gives you no
reference to verify AEC against, and the 6-channel variant exposes raw capsules
we must not process on the Pi and breaks `/etc/asound.conf`.

Then, five seconds after the reset:

```bash
lsusb                 # expect a Seeed device, vendor id 2886
dmesg | tail -30      # expect USB Audio Class / snd-usb-audio binding
aplay -l              # expect exactly ONE card
arecord -l            # expect the same card for capture
```

- **PASS:** `aplay -l` and `arecord -l` each list exactly one card, and it is
  the Flex. Write down the **short name in square brackets** — e.g. `Flex` from
  `card 0: Flex [ReSpeaker Flex], device 0: ...`. That name (not the index)
  goes into `hermit.card` in `/etc/asound.conf`; USB enumeration order is not
  stable across reboots, the name is.
- **FAIL:** no card listed → still on I2S firmware, or a cable/power problem.
  See the troubleshooting section of `flash_notes.md`. Do not continue.
- **FAIL:** more than one card listed → the onboard bcm2835 or HDMI audio is
  still registering. Run `provision.sh` (it sets `dtparam=audio=off`, adds
  `noaudio` to the vc4-kms-v3d overlay and blacklists `snd_bcm2835`), reboot,
  and re-check.

Record: **card short name**.

---

## Step 2 — Capabilities: what does the card actually support?

This is the most consequential measurement in Phase 0. Substitute your card's
short name for `Flex` throughout.

```bash
# Capture capabilities
arecord -D hw:CARD=Flex,0 --dump-hw-params -d 1 /dev/null

# Playback capabilities
aplay -D hw:CARD=Flex,0 --dump-hw-params /dev/zero
```

Read three lines out of each dump and write them down:

| Line | What to record | What it decides |
|---|---|---|
| `CHANNELS` (capture) | must be **2** | 6 = wrong firmware variant, reflash. 1 = wrong variant, reflash. `hermit_dsnoop` in asound.conf declares `channels 2`. |
| `RATE` | the native rate(s) | **Everything below.** |
| `FORMAT` | normally `S16_LE` | if only `S32_LE`, set `hermit.format` accordingly |
| `CHANNELS` (playback) | 1 or 2 | sets `hermit.play_channels`; if 1, switch the downmix ttable in asound.conf to the mono variant |

**Why the rate matters so much.** It propagates into four decisions:

1. **What we ask the TTS provider for.** Cartesia and ElevenLabs will both
   return raw PCM at a requested rate. Asking for the card's native rate means
   the bytes go from the socket to ALSA with **no resampling anywhere** — the
   lowest-latency and highest-quality path. Asking for the wrong one means
   `plug` resamples every TTS sentence on the Pi's CPU, on the hot path,
   forever.
   - If the card is natively **16000** → request 16 kHz PCM. This is the
     expected case for a 16 kHz XVF3800 pipeline.
   - If the card offers **48000** → request 48 kHz PCM and set `hermit.rate
     48000`. dmix then runs at 48 k natively.
2. **Whether librespot and mpv need `plug` resampling.** Spotify decodes at
   44.1 kHz and most radio streams are 44.1 or 48 kHz. If the card is 16 kHz
   only, every stream is resampled down — audible, unavoidable, and fine for
   this speaker, but know that it is happening and that it costs CPU.
3. **`hermit.rate` in `/etc/asound.conf`**, which sets the dmix/dsnoop slave
   rate. dmix does not resample; get this wrong and you pay for a pointless
   conversion on every stream.
4. **The period/buffer sizes** in the same adjustment block — the file gives a
   known-good pair for each rate. Uncomment the matching one.

Now edit the top of `/etc/asound.conf` (and the copy in
`hermit/deploy/asound.conf`, so the next build of this device is right first
time) and set `hermit.card`, `hermit.rate`, and if needed `hermit.format` and
`hermit.play_channels`.

- **PASS:** capture reports exactly 2 channels; you have written down the rate
  and format; `/etc/asound.conf` has been updated to match.
- **FAIL:** capture reports 6 or 1 channels → wrong firmware. Return to step 1.

Record: **capture channels, native rate(s), format, playback channels**, and
the decision **"TTS provider will be asked for _____ Hz PCM"**.

---

## Step 3 — Prove capture and playback

Run `provision.sh` first if you have not: it installs `/etc/asound.conf` and
raises the XVF3800's playback level, which comes up far too quiet on Linux and
will otherwise have you convinced the amplifier is dead.

```bash
sudo /path/to/hermit/deploy/provision.sh
```

### 3a. Capture, raw hardware

```bash
arecord -D plughw:CARD=Flex,0 -c 2 -r 16000 -f S16_LE -d 5 /tmp/raw2ch.wav
aplay /tmp/raw2ch.wav
```

Speak normally at about 50 cm while it records.

- **PASS:** voice is clearly audible and intelligible; no continuous hiss, buzz
  or dropouts.
- **FAIL, silence:** check `alsamixer -c Flex` capture levels; check the FPC
  cable between the mic strip and the core board is fully seated.

### 3b. Capture, through the configured chain

This is the PCM the daemon will actually open.

```bash
arecord -D hermit_in -f S16_LE -r 16000 -c 1 -d 5 /tmp/voice.wav
aplay /tmp/voice.wav
```

- **PASS:** opens without error, produces a **mono** file, voice is clear.
- **FAIL, "Unknown PCM hermit_in":** `/etc/asound.conf` is missing or has a
  syntax error. `arecord -D hermit_in ...` prints the parse error; the usual
  cause is a `hermit.card` name that does not match any present card.

### 3c. Playback

```bash
speaker-test -D hermit_out -c 2 -t sine -f 440 -l 1
aplay -D hermit_out /usr/share/sounds/alsa/Front_Center.wav
```

- **PASS:** a clean 440 Hz tone from the FIT0502, and intelligible speech from
  the WAV. No buzzing, no crackle, no clipping.
- **FAIL, silence:** `alsamixer -c Flex` — raise `PCM-1` (or `PCM`) to 100 %
  and check nothing is muted; then `sudo alsactl store`. Confirm the speaker is
  on the **JST amplifier output of the core board**, not the aux jack.
- **FAIL, distortion at moderate volume:** you are clipping the amp or
  overdriving the 3 W speaker. Confirm the softvol ceiling is in place:
  `amixer -c Flex sget "Hermit Master"` should exist and read 100 %, which is
  the **-2.5 dBFS** ceiling, not full scale. Compare against
  `speaker-test -D hermit_raw ...` — `hermit_raw` bypasses the ceiling and
  should be audibly louder. If they are the same volume, the softvol stage is
  not in the chain and the speaker is unprotected.

### 3d. Simultaneous open (dmix / dsnoop sanity)

Three processes must be able to use the card at once — the daemon, librespot
and mpv all will.

```bash
aplay -D hermit_out /usr/share/sounds/alsa/Noise.wav &
speaker-test -D hermit_out -c 2 -t sine -f 880 -l 1
arecord -D hermit_in -f S16_LE -r 16000 -c 1 -d 3 /tmp/concurrent.wav
wait
```

- **PASS:** all three run together; nothing reports "Device or resource busy".
- **FAIL, "resource busy":** dmix/dsnoop are not being used — check for a typo
  in `/etc/asound.conf`, and that nothing is opening `hw:` directly.

---

## Step 4 — AEC sanity test (the one that decides whether barge-in works)

This is the whole reason for the locked topology. If the XVF3800's hardware AEC
is working, music played through the Flex is strongly suppressed in the
processed capture channel, so the wake word engine still hears you over it. If
it is not working, everything downstream appears to function and barge-in
silently does not — the worst possible failure mode, because it only shows up
in real use.

### Method

Play a continuous, broadband signal through the Flex, and simultaneously record
**both** the processed channel and the AEC reference channel. **Say nothing.**
`dsnoop` is what allows both recordings at once.

```bash
cd /tmp

# 1. Start music/noise on the speaker at a normal listening level.
speaker-test -D hermit_out -c 2 -t pink > /dev/null 2>&1 &
SPK=$!
sleep 2                       # let AEC converge — it needs a moment

# 2. Record both channels at the same time, in silence.
arecord -D hermit_in    -f S16_LE -r 16000 -c 1 -d 10 aec_processed.wav &
arecord -D capture_ref  -f S16_LE -r 16000 -c 1 -d 10 aec_reference.wav &
wait %2 %3

kill $SPK 2>/dev/null
```

(Music is a better test than pink noise if you have a WAV to hand — real
program material is non-stationary, which is harder for an AEC. Use
`aplay -D hermit_out music.wav &` in place of `speaker-test`.)

### Listening check

```bash
aplay aec_reference.wav    # should be OBVIOUSLY the music/noise, loud
aplay aec_processed.wav    # should be near-silence, or a faint residue
```

### Numeric check

Measure the RMS of each file and take the difference in dB. Pure stdlib, no
extra packages:

```bash
python3 - <<'PY'
import wave, struct, math

def rms_dbfs(path):
    with wave.open(path, 'rb') as w:
        assert w.getsampwidth() == 2, "expected 16-bit"
        n = w.getnframes()
        data = w.readframes(n)
    s = struct.unpack('<%dh' % (len(data) // 2), data)
    if not s:
        return float('-inf')
    rms = math.sqrt(sum(x * x for x in s) / len(s))
    return 20 * math.log10(rms / 32768.0) if rms > 0 else float('-inf')

ref  = rms_dbfs('aec_reference.wav')
proc = rms_dbfs('aec_processed.wav')
print(f"reference (ch1) : {ref:7.2f} dBFS")
print(f"processed (ch0) : {proc:7.2f} dBFS")
print(f"suppression     : {ref - proc:7.2f} dB")
PY
```

- **PASS:** suppression **>= 20 dB**. Hardware AEC is working; barge-in will
  work.
- **MARGINAL: 12–20 dB.** Usually mechanical. The speaker is coupling into the
  mic PCB through the chassis (structure-borne sound cannot be cancelled), or
  the playback level is high enough to drive the amplifier into non-linearity
  (an AEC can only cancel a *linear* echo path). Improve the foam decoupling,
  increase the distance between speaker and mic strip, lower the level, re-run.
- **FAIL: < 12 dB.** Investigate before going further:
  1. Are you certain playback went through the Flex? If any audio reached the
     Pi's 3.5 mm jack or an HDMI sink, the XVF3800 never saw it and has nothing
     to cancel. `aplay -l` must show exactly one card.
  2. Is the mic strip's FPC fully seated?
  3. Did you actually flash the **USB 2-channel** firmware? (step 1)
  4. Was the room silent during the recording?

### Note on channel 1

Seeed's firmware line has shipped more than one meaning for channel 1 across
revisions — some 2-channel builds put a second processed beam (an "ASR" output)
there rather than the raw AEC reference. Confirm empirically which you have: if
`aec_reference.wav` contains the music loudly, it is behaving as a reference.
If *both* files are near-silent, channel 1 is a second processed stream on your
firmware — in that case compare `aec_processed.wav` against a recording made
with the same music playing but captured from `plughw:` channel 1 directly, or
simply against the known playback level.

Either way the criterion that matters is unchanged and is a real-world one:

### Real barge-in check

```bash
aplay -D hermit_out music.wav &
arecord -D hermit_in -f S16_LE -r 16000 -c 1 -d 10 barge.wav
# Speak your wake phrase, at a normal voice, from ~2 m, while the music plays.
aplay barge.wav
```

- **PASS:** your voice is clearly intelligible in `barge.wav` and the music is
  a faint background. That is what the wake word engine will be given.
- **FAIL:** your voice is buried under the music → do not proceed to build the
  wake-word phase on top of this. Fix the acoustics or the firmware first.

Record: **suppression in dB**, and **barge-in pass/fail**.

---

## Step 5 — Measure real Cerebras time-to-first-token from the Pi

Measured **from the Pi, on the real network**, not from your laptop. This is
the number the entire latency budget is built on, and Wi-Fi vs Ethernet vs your
ISP's routing to Cerebras will move it more than anything in the code.

```bash
export CEREBRAS_API_KEY='...'   # or: set -a; . /etc/hermit/hermit.env; set +a
```

### Single measurement

```bash
curl -sS -N -o /dev/null \
  -w 'dns=%{time_namelookup}s tcp=%{time_connect}s tls=%{time_appconnect}s TTFT=%{time_starttransfer}s total=%{time_total}s\n' \
  -X POST https://api.cerebras.ai/v1/chat/completions \
  -H "Authorization: Bearer ${CEREBRAS_API_KEY}" \
  -H 'Content-Type: application/json' \
  -d '{
        "model": "gpt-oss-120b",
        "stream": true,
        "reasoning_effort": "low",
        "max_completion_tokens": 64,
        "messages": [
          {"role":"system","content":"You are a terse voice assistant. One short sentence."},
          {"role":"user","content":"What is the capital of Australia?"}
        ]
      }'
```

With `"stream": true` and `-N` (no buffering), `time_starttransfer` is the time
to the **first byte of the response body** — i.e. the first SSE chunk — which is
time-to-first-token. Without `stream`, the same field would measure the time to
the *complete* answer and be meaningless here.

`reasoning_effort: "low"` is what the interactive path uses. `gpt-oss-120b`
emits reasoning tokens before content; at `low` that preamble is short, but it
is part of what you are measuring and it is the right thing to measure, because
it is what the user waits through.

### Ten runs, warm and cold, with a median

A single sample is noise. Also separate **cold** TTFT (includes DNS + TCP + TLS,
~100–200 ms) from **warm** TTFT (what the daemon actually sees, since it holds a
pooled keep-alive connection):

```bash
cat > /tmp/ttft.sh <<'SH'
#!/bin/bash
set -euo pipefail
: "${CEREBRAS_API_KEY:?set CEREBRAS_API_KEY first}"
N=${1:-10}
echo "run  cold_TTFT  tls_handshake_done  warm_equiv"
for i in $(seq 1 "$N"); do
  read -r ttft app <<<"$(curl -sS -N -o /dev/null \
    -w '%{time_starttransfer} %{time_appconnect}' \
    -X POST https://api.cerebras.ai/v1/chat/completions \
    -H "Authorization: Bearer ${CEREBRAS_API_KEY}" \
    -H 'Content-Type: application/json' \
    -d '{"model":"gpt-oss-120b","stream":true,"reasoning_effort":"low",
         "max_completion_tokens":64,
         "messages":[{"role":"system","content":"You are a terse voice assistant. One short sentence."},
                     {"role":"user","content":"What is the capital of Australia?"}]}')"
  warm=$(awk -v a="$ttft" -v b="$app" 'BEGIN{printf "%.3f", a-b}')
  printf '%3d  %9.3f  %18.3f  %9.3f\n' "$i" "$ttft" "$app" "$warm"
  echo "$ttft $warm" >> /tmp/ttft.raw
  sleep 1
done
echo
echo -n "median cold TTFT: "; awk '{print $1}' /tmp/ttft.raw | sort -n | awk '{a[NR]=$1} END{print (NR%2? a[(NR+1)/2] : (a[NR/2]+a[NR/2+1])/2) "s"}'
echo -n "median warm TTFT: "; awk '{print $2}' /tmp/ttft.raw | sort -n | awk '{a[NR]=$1} END{print (NR%2? a[(NR+1)/2] : (a[NR/2]+a[NR/2+1])/2) "s"}'
SH
chmod +x /tmp/ttft.sh
rm -f /tmp/ttft.raw
/tmp/ttft.sh 10
```

Run it once on **Ethernet** and, if the device may ever be on Wi-Fi, once on
Wi-Fi. Record both.

- **PASS:** the request returns 200 and streams; median **warm** TTFT is
  recorded. On Ethernet from a reasonable connection, expect a few hundred
  milliseconds. What matters is that you have the *actual* number, not that it
  hits any particular target.
- **FAIL, HTTP 401:** bad or missing key.
- **FAIL, TLS error:** the clock is wrong. A Pi has no RTC and boots in 1970
  until NTP lands. `timedatectl status`, and confirm `provision.sh` enabled
  time sync.
- **FAIL, TTFT above ~1.5 s on Ethernet:** something is wrong with the network
  path, not with Cerebras. Compare against the same command from your laptop on
  the same LAN; if the laptop is fast and the Pi is slow, look at the Pi's
  route, DNS resolver and whether it fell back to Wi-Fi.

Record: **median cold TTFT, median warm TTFT, link type**.

---

## Step 6 — Thermals under sustained load

`provision.sh` pins the CPU governor to `performance`, which trades a few
degrees for the removal of tens of milliseconds of governor ramp-up latency on
every wake. That is the right trade here, but it makes the enclosure's thermal
design load-bearing.

Baseline, idle:

```bash
vcgencmd measure_temp
vcgencmd get_throttled
```

Then load all four cores for ten minutes **with the enclosure closed**, exactly
as it will run:

```bash
for i in 1 2 3 4; do yes > /dev/null & done
end=$((SECONDS+600))
while [ $SECONDS -lt $end ]; do
  printf '%s  %s  %s\n' "$(date +%T)" "$(vcgencmd measure_temp)" "$(vcgencmd get_throttled)"
  sleep 15
done
kill %1 %2 %3 %4
vcgencmd get_throttled
```

Reading `get_throttled` — it is a bitmask, and the high bits are sticky, i.e.
they record that something happened at some point since boot:

| Bit | Meaning |
|---|---|
| `0x0` | nothing has ever happened. This is what you want. |
| `0x1` | under-voltage **now** — power supply or cable |
| `0x4` | ARM frequency capped now |
| `0x8` | soft temperature limit active now |
| `0x10000` | under-voltage **has occurred** since boot |
| `0x40000` | throttling **has occurred** since boot |
| `0x80000` | soft temperature limit has occurred since boot |

- **PASS:** peak temperature stays **below 80 °C** across the ten minutes, and
  `get_throttled` reads `0x0` at the end.
- **FAIL, above 80 °C or throttling bits set:** add or improve the heatsink,
  add vents, or fit a 25 mm fan. Sustained throttling means the CPU drops clock
  exactly when the device is busy, which is precisely when latency matters.
- **FAIL, any under-voltage bit (`0x1` / `0x10000`):** this is a **power**
  fault, not thermal. Use the official 5.1 V / 3 A PSU. If it persists with the
  speaker driven hard, move the Flex off bus power and onto its separate 5 V
  JST / 12 V terminal input.

Repeat the ten-minute run **with music playing through the speaker at normal
volume**, since the amplifier's draw is what pushes a marginal supply over.

Record: **idle temp, peak temp under load, `get_throttled` value**.

---

## THE GATE

Phase 0 is complete, and later phases may begin, when **all three** of these
are true and written down:

1. **The card enumerates.** USB 2-channel firmware is flashed; `aplay -l` and
   `arecord -l` show exactly one card, the Flex; capture is 2 channels; the
   native rate is recorded and `/etc/asound.conf` matches it.
2. **Loopback AEC works.** Music played through the Flex is suppressed by
   **>= 20 dB** in the processed capture channel, and a spoken phrase is
   clearly intelligible in a recording made while music plays.
3. **TTFT is measured and recorded.** A real median warm time-to-first-token
   from this Pi, on this network, to `api.cerebras.ai` with `gpt-oss-120b` at
   `reasoning_effort=low`.

If any of the three is not met, stop. Every phase after this one assumes all
three, and debugging them later — through a daemon, a wake-word engine and a
streaming TTS pipeline — is dramatically harder than debugging them here with
`arecord` and `curl`.

---

## Results — session log (2026-08-18, <pi-host>)

Findings recorded live during bring-up. Anything still blank is genuinely not yet
measured; do not treat a blank as a pass.

### Environment (differs from the assumptions in this doc)

| Field | Value | Note |
|---|---|---|
| Device | Raspberry Pi 4 Model B Rev 1.5, 905 MB usable | |
| OS | **Debian 13 (trixie)**, not Bookworm | binary built on bookworm (glibc 2.36) runs fine on 2.41 — forward compat |
| Network | **wlan0**, <redacted-lan-ip>; eth0 DOWN | spec prefers Ethernet; Wi-Fi jitter is in every number below |
| Power | under-voltage logged at boot (`0x50005` → `0x50000`) | 40 °C, so power not thermal. Confirm 5.1 V/3 A PSU. |
| dpkg state | interrupted dist-upgrade found; repaired with `dpkg --configure -a` | caused by the power loss; blocked ALL package installs |

### Step 0 — Board enumeration BEFORE flashing

| Field | Value |
|---|---|
| `lsusb` | `2886:801c Seeed Technology Co., Ltd. reSpeaker XVF3800 Safe Mode` |
| USB path | `usb 1-1.4`, SerialNumber 100026178261700018 |
| Audio cards present | **none from the board** — only bcm2835 Headphones + 2× vc4hdmi |
| `arecord -l` | empty |
| Conclusion | ships I2S firmware as the spec predicted; USB flash is REQUIRED |

Note: the DFU product id observed is **`2886:801c`**, not the `2886:001a` quoted on the
Seeed wiki. Trust what `lsusb` reports on the day.

### Cerebras TTFT from this Pi (gpt-oss-120b, reasoning_effort=low, stream=true)

| Method | p50 | Note |
|---|---|---|
| Raw `curl`, fresh TLS each call | **497 ms** | 6 runs, 452–607 ms |
| Daemon with pre-warmed pool | **366 ms** | 20-request bench |
| **Saved by connection pre-warming** | **~130 ms** | validates spec §5 on hardware |

RTT to api.cerebras.ai: 16 ms avg (9–26 ms, Wi-Fi).

### Phase 1 gate — PASSED on device

| Gate | p50 | p95 | Target | |
|---|---|---|---|---|
| Local overhead (route+recall+assemble) | 1.1 ms | 1.9 ms | ≤ 15 ms | PASS |
| Text TTFT, no tools | 366 ms | 384 ms | < 700 ms | PASS |
| Fast-path device command | 1.0 ms | 1.6 ms | < 50 ms | PASS |

Command: `bench.sh --bin ~/hermit-deploy/bin/hermit --config ~/hermit-test/hermit.toml --runs 20`

### BLOCKER — inadequate power supply (2026-08-18)

Bring-up is halted here. The Pi cannot hold its 5 V rail:

```
hwmon hwmon1: Undervoltage detected!
hwmon hwmon1: Voltage normalised
... alternating continuously, every 10-20 s, at 43 C
```

`vcgencmd get_throttled` oscillates between `0x50000` and `0x50005` (bit 0 =
under-voltage now, bit 2 = throttled now). Temperature is 43 C, so this is supply,
not thermal.

Observed consequences, in order of discovery:
1. Pi dropped off the network three times, twice under load.
2. A previous power loss left dpkg mid-transaction (`libc-bin`, `curl`, `gpg` and
   ~15 others unpacked-but-unconfigured), which blocked ALL package installation
   until repaired with `dpkg --configure -a`.
3. **USB audio capture fails outright**: `arecord` returns
   `pcm_read: read error: Input/output error` with 0 frames captured, on `hw:`,
   `plughw:`, and with explicit period/buffer sizing. The device stays enumerated
   in `lsusb` throughout.

The capture failure is not yet *proven* to be caused by the brownouts — a firmware
or driver quirk cannot be excluded — but USB peripherals are the first thing to
fail under a sagging 5 V rail, and no diagnosis is trustworthy until the supply is
stable. Do not chase the audio bug before fixing power.

**Required:** the official Raspberry Pi 5.1 V / 3 A USB-C PSU, and a short, thick
USB-C cable. Feeding the Flex from its own 5 V JST / 12 V input (spec §2) reduces
the Pi's downstream USB load and demonstrably helped — `throttled` briefly cleared
to `0x50000` — but did not fix it, because the under-voltage was present at first
boot with NO USB device attached at all.

**Risk if ignored:** repeated brownouts during SD writes risk filesystem corruption.
One interrupted dist-upgrade has already happened.

### Phases 6 + 7 — VERIFIED against the real model (dev machine, Cerebras only)

These need no audio hardware, so they were validated while the Pi was down.

**Tool calling against real gpt-oss-120b** (not a stub): the model emitted a
`web_search` call, the streaming accumulator reassembled the fragmented SSE deltas
with no parse warnings, the tool error (no Parallel key) was fed back, and the model
degraded gracefully rather than failing the turn. `tool_rounds=1`, cap respected.

**Learning loop.** Four conversational turns stating personal facts, then the
reflection nudge fired twice and stored 4 facts:

| Fact extracted | Tags | Importance |
|---|---|---|
| The user lives in Bergen, Norway. | location | 0.9 |
| The user works night shifts and requests no news before 2 p.m. | schedule,news,preference | 0.9 |
| The user has a border collie dog named Ada. | pet | 0.6 |
| The user prefers to use metric units. | preference | 0.6 |

All carry `source = reflection`. Standing instructions and identity correctly scored
above incidental detail, exactly as the extraction prompt asks.

**Cross-session recall.** A NEW session (fresh session_id, empty history) against the
same database answered from memory alone:

```
what is my dog's name?                        -> Your dog's name is Ada.
what units should you use when talking to me? -> I should use metric units when speaking with you.
where do I live?                              -> You live in Bergen, Norway.
```

Recall took **0.7 ms** (gate: < 5 ms).

**Firewall held**: `select count(*) from messages where role not in ('user','assistant')`
returned **0** — no tool or web content reached the transcript that reflection reads.

### Still to measure

- [ ] Post-flash enumeration, `--dump-hw-params` (sets `tts.sample_rate`)
- [ ] AEC suppression (ch0 vs ch1)
- [ ] First-audio gates (needs TTS key)
- [ ] Wake→listening, barge-in flush
- [ ] 24 h thermal soak

---

## Results — fill this in

Date: ______________  Operator: ______________  Pi serial: ______________

### Step 1 — Enumeration

| Field | Value |
|---|---|
| Firmware file flashed | `respeaker_flex_usb_l16k2ch_v1.0.3.bin` (or: __________) |
| `dfu-util` alt setting used | |
| Card index from `aplay -l` | |
| **Card short name** (→ `hermit.card`) | |
| USB VID:PID after flash | |
| Any other card present? (must be none) | |

### Step 2 — Capabilities

| Field | Capture | Playback |
|---|---|---|
| CHANNELS | (must be 2) | |
| RATE(s) | | |
| FORMAT | | |
| PERIOD_SIZE range | | |

| Decision | Value |
|---|---|
| **`hermit.rate` set to** | |
| **`hermit.format` set to** | |
| **`hermit.play_channels` set to** | |
| **TTS provider will be asked for** | ______ Hz PCM |
| Does librespot/mpv need `plug` resampling? | yes / no |

### Step 3 — Capture and playback

| Check | Result |
|---|---|
| 3a raw 2-channel capture intelligible | pass / fail |
| 3b `hermit_in` opens, mono, clear | pass / fail |
| 3c `hermit_out` tone clean, no clipping | pass / fail |
| 3c `hermit_raw` audibly louder than `hermit_out` (ceiling is active) | pass / fail |
| 3d three simultaneous opens (dmix/dsnoop) | pass / fail |
| Mixer controls found and stored (`alsactl store`) | |

### Step 4 — AEC

| Field | Value |
|---|---|
| Test signal used (pink noise / music) | |
| Reference channel RMS | ________ dBFS |
| Processed channel RMS | ________ dBFS |
| **Suppression** | ________ dB (pass = >= 20) |
| Channel 1 behaves as a reference? | yes / no (second processed beam) |
| Real barge-in: voice intelligible over music | pass / fail |
| Speaker mechanically decoupled | yes / no |

### Step 5 — Cerebras TTFT

Model `gpt-oss-120b`, `reasoning_effort=low`, endpoint `api.cerebras.ai`.

| Field | Ethernet | Wi-Fi (if applicable) |
|---|---|---|
| Runs | | |
| Median **cold** TTFT (incl. DNS/TCP/TLS) | ________ s | ________ s |
| Median **warm** TTFT (pooled connection) | ________ s | ________ s |
| Min / max warm TTFT | ____ / ____ s | ____ / ____ s |
| HTTP status | | |

### Step 6 — Thermals

| Field | Value |
|---|---|
| Idle temperature | ________ °C |
| Peak temp, 10 min 4-core load, enclosure closed | ________ °C |
| Peak temp, same load + music playing | ________ °C |
| `get_throttled` after the runs | `0x________` (pass = `0x0`) |
| Heatsink / fan fitted | |
| Flex powered from: bus / separate 5 V JST / 12 V | |

### Gate

| Gate condition | Met? |
|---|---|
| 1. Card enumerates, 2 channels, rate recorded | yes / no |
| 2. Loopback AEC >= 20 dB and barge-in intelligible | yes / no |
| 3. TTFT measured and recorded | yes / no |

**Phase 0 signed off:** ______________  **Date:** ______________
