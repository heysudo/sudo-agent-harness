#!/usr/bin/env bash
# Phase 0 hardware bring-up collector. RUNS ON THE PI.
#
# Gathers every fact docs/BRINGUP.md asks the operator to record, into one report,
# so the numbers that later phases depend on (card index, native rate, channel
# count) are captured verbatim rather than transcribed by hand at 1am.
#
# Usage:  bash phase0.sh [--aec] [--ttft]
#   --aec   run the AEC sanity test (plays a tone while recording; needs the
#           speaker connected and the room reasonably quiet)
#   --ttft  measure Cerebras time-to-first-token from this Pi (needs
#           CEREBRAS_API_KEY in the environment or /etc/hermit/hermit.env)
#
# Safe to run repeatedly. Nothing here changes system state.

set -uo pipefail

RUN_AEC=0; RUN_TTFT=0
for a in "$@"; do
  case "$a" in
    --aec)  RUN_AEC=1 ;;
    --ttft) RUN_TTFT=1 ;;
  esac
done

OUT="$HOME/phase0-$(date +%Y%m%d-%H%M%S).txt"
exec > >(tee "$OUT") 2>&1

hr() { printf '\n%s\n%s\n' "== $1" "$(printf '%.0s-' {1..70})"; }

hr "system"
uname -a
grep PRETTY /etc/os-release
echo "kernel cmdline: $(cat /proc/cmdline | tr ' ' '\n' | grep -E 'audio|snd' | tr '\n' ' ')"
echo "mem: $(free -m | awk '/Mem:/{print $2" MB total, "$7" MB available"}')"
echo "temp: $(vcgencmd measure_temp 2>/dev/null || echo n/a)   throttled: $(vcgencmd get_throttled 2>/dev/null || echo n/a)"
echo "governor: $(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor 2>/dev/null || echo n/a)"

hr "network path"
ip -br addr | grep -v "^lo"
DEF_IF=$(ip route show default | awk '{print $5}' | head -1)
echo "default via: $DEF_IF"
if [[ "$DEF_IF" == wl* ]]; then
  echo "WARNING: default route is Wi-Fi. Latency numbers will carry Wi-Fi jitter; the spec wants Ethernet."
fi
echo "ping api.cerebras.ai:"
ping -c 5 -i 0.3 api.cerebras.ai 2>&1 | tail -2

hr "USB devices"
lsusb
echo
echo "XVF3800 / reSpeaker candidates:"
lsusb | grep -iE "2886|seeed|respeaker|xmos|20b1" || echo "  (none matching 2886:*, seeed, respeaker, xmos, 20b1:*)"
echo
if lsusb | grep -q "2886:001a"; then
  echo "NOTE: 2886:001a is the DFU/bootloader ID — the board is in DFU mode right now."
fi

hr "ALSA cards"
echo "--- aplay -l"; aplay -l 2>&1
echo "--- arecord -l"; arecord -l 2>&1
echo "--- /proc/asound/cards"; cat /proc/asound/cards 2>&1

# Find the reSpeaker card index for the hw-params dump.
CARD=$(aplay -l 2>/dev/null | grep -iE "respeaker|xvf|seeed|xmos|usb audio" | head -1 | sed -E 's/^card ([0-9]+):.*/\1/')
if [[ -z "${CARD:-}" ]]; then
  # Fall back to the first USB card of any name.
  CARD=$(grep -iE "usb" /proc/asound/cards 2>/dev/null | head -1 | awk '{print $1}')
fi

if [[ -n "${CARD:-}" ]]; then
  echo
  echo "candidate card index: $CARD  ($(sed -n "s/^ *$CARD \[\([^]]*\)\].*/\1/p" /proc/asound/cards | head -1))"

  hr "playback hw params (card $CARD)  <-- THIS sets tts.sample_rate / audio.sample_rate"
  aplay -D hw:$CARD,0 --dump-hw-params /dev/zero 2>&1 | sed -n '/^ACCESS/,/^$/p' | head -20 || true
  # aplay exits non-zero after dumping; that is expected.

  hr "capture hw params (card $CARD)"
  timeout 2 arecord -D hw:$CARD,0 --dump-hw-params /dev/null 2>&1 | sed -n '/^ACCESS/,/^$/p' | head -20 || true

  hr "mixer controls (card $CARD)"
  amixer -c $CARD scontrols 2>&1
  amixer -c $CARD 2>&1 | grep -E "Simple mixer|Limits|Playback|Capture" | head -30

  hr "usb descriptor summary (card $CARD)"
  cat /proc/asound/card$CARD/stream0 2>/dev/null | head -60 || echo "no stream0"
else
  echo
  echo "NO USB AUDIO CARD FOUND."
  echo "If lsusb above shows nothing from vendor 2886, the board is either unplugged,"
  echo "unpowered, or still on I2S firmware (invisible over USB). See scripts/flash_notes.md."
fi

if [[ $RUN_AEC -eq 1 && -n "${CARD:-}" ]]; then
  hr "AEC sanity test"
  echo "Playing a 3s 440Hz tone through the Flex while recording both capture channels."
  echo "Pass = channel 0 (processed) shows the tone strongly attenuated vs channel 1 (reference)."
  T=/tmp/aec-test
  rm -f $T-*.wav
  # Record 2ch for 4s in the background, start the tone 0.5s in.
  ( arecord -D hw:$CARD,0 -f S16_LE -r 16000 -c 2 -d 4 $T-rec.wav >/dev/null 2>&1 ) &
  REC=$!
  sleep 0.5
  speaker-test -D plughw:$CARD,0 -t sine -f 440 -l 1 -p 3000 -c 1 >/dev/null 2>&1 || \
    speaker-test -D plughw:$CARD,0 -t sine -f 440 -l 1 -c 2 >/dev/null 2>&1
  wait $REC
  if [[ -f $T-rec.wav ]]; then
    # Split channels and compute RMS with sox if present, else python.
    if command -v sox >/dev/null; then
      sox $T-rec.wav $T-ch0.wav remix 1 && sox $T-rec.wav $T-ch1.wav remix 2
      echo "ch0 (processed) RMS: $(sox $T-ch0.wav -n stat 2>&1 | awk '/RMS *amplitude/{print $3}')"
      echo "ch1 (reference) RMS: $(sox $T-ch1.wav -n stat 2>&1 | awk '/RMS *amplitude/{print $3}')"
    else
      python3 - "$T-rec.wav" <<'PY'
import sys, wave, struct, math
w = wave.open(sys.argv[1]); n = w.getnframes(); ch = w.getnchannels()
raw = w.readframes(n); s = struct.unpack("<%dh" % (n*ch), raw)
def rms(xs): return math.sqrt(sum(x*x for x in xs)/max(1,len(xs)))/32768.0
c0 = s[0::ch]; c1 = s[1::ch] if ch > 1 else []
r0 = rms(c0); r1 = rms(c1) if c1 else float('nan')
print(f"ch0 (processed) RMS: {r0:.5f}")
print(f"ch1 (reference) RMS: {r1:.5f}")
if c1 and r0 > 0:
    print(f"suppression: {20*math.log10(r1/r0):.1f} dB  (pass if > ~15 dB)")
PY
    fi
    echo "recording kept at $T-rec.wav — scp it back and listen if the numbers are ambiguous."
  else
    echo "recording failed"
  fi
fi

if [[ $RUN_TTFT -eq 1 ]]; then
  hr "Cerebras TTFT from this Pi (reasoning_effort=low, stream=true)"
  KEY="${CEREBRAS_API_KEY:-}"
  if [[ -z "$KEY" && -r /etc/hermit/hermit.env ]]; then
    KEY=$(grep -E '^CEREBRAS_API_KEY=' /etc/hermit/hermit.env | cut -d= -f2-)
  fi
  if [[ -z "$KEY" ]]; then
    echo "CEREBRAS_API_KEY not set; skipping. export it or fill /etc/hermit/hermit.env"
  else
    for i in 1 2 3 4 5; do
      # time_starttransfer = first byte of the response body = first SSE frame.
      curl -sS -o /dev/null -w "run $i: connect=%{time_connect}s tls=%{time_appconnect}s TTFB=%{time_starttransfer}s total=%{time_total}s\n" \
        -N https://api.cerebras.ai/v1/chat/completions \
        -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
        -d '{"model":"gpt-oss-120b","stream":true,"reasoning_effort":"low","max_completion_tokens":40,"messages":[{"role":"user","content":"Say hello in five words."}]}'
    done
    echo "NOTE: run 1 pays the TLS handshake; runs 2-5 reflect the pre-warmed steady state hermit runs in."
  fi
fi

hr "done"
echo "report saved: $OUT"
