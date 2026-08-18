# Flashing the reSpeaker Flex XVF3800 to USB firmware

**This is step 1 of hardware bring-up. Nothing else works until it is done.**

Authoritative source: the Seeed Studio wiki.

- <https://wiki.seeedstudio.com/respeaker_flex_introduction/>
- <https://wiki.seeedstudio.com/respeaker_flex_xiao_introduction/> (our SKU)
- Firmware assets and the XMOS DFU background are linked from those pages.

> **If the wiki and this file disagree, the wiki wins.** Seeed revises the
> board, the button silkscreen and the firmware filenames between production
> runs. These notes record what worked and *why* each step exists; they are not
> a substitute for reading the current page before you plug anything in.

---

## 1. Why flashing is mandatory for this SKU

We have the **reSpeaker Flex XVF3800 Linear-4, "with XIAO ESP32S3" SKU**.

That SKU ships with the **I2S firmware** loaded. The I2S firmware exists so the
board can feed a microcontroller host (the bundled XIAO ESP32S3) over I2S. It
does not implement USB Audio Class at all.

The practical consequence, and the thing that will waste your evening if you
do not know it:

> **Out of the box this board is invisible to Linux as an audio device.**
> `lsusb` may show *something*, but `arecord -l` and `aplay -l` will show **no
> card**. This is not a broken board, a bad cable, a permissions problem or a
> missing kernel module. It is the wrong firmware.

HERMIT's topology needs the Flex to be the Pi's one and only sound card — mic
capture *and* all playback through the same device, so the XVF3800's hardware
AEC always has a loopback reference and barge-in works. That requires the
**USB (UAC 2.0)** firmware.

The XIAO ESP32S3 is **not used** in this build. Leave it seated and idle. (A
plausible future use is driving the LED ring for direction-of-arrival display;
it has no role in the audio path and must not acquire one.)

### Which variant: the 2-channel USB firmware

The USB firmware comes in more than one channel layout. **Flash the 2-channel
linear variant.**

| Variant | Channels | Use it? |
|---|---|---|
| **USB 2-channel, linear** | ch0 = processed mono voice (AEC + beamforming + noise suppression + dereverb + AGC applied on-chip), ch1 = AEC reference | **YES — this is the one** |
| USB 6-channel | ch0–1 processed, ch2–5 raw per-microphone | No |
| USB 2-channel, **circular** | same layout, wrong array geometry | No |
| I2S | for a microcontroller host over I2S | No — this is what ships, and what we are replacing |

Why 2-channel and not the others:

- **Channel 0 is the only audio the Pi should ever process.** All the DSP has
  already happened on the XMOS chip at 16 kHz. The Pi does no beamforming, no
  noise suppression and no echo cancellation — it just moves buffers. That is
  the entire reason this board was chosen.
- **Channel 1 is the AEC reference**, i.e. a loopback of what the board is
  playing. We do not use it at runtime, but it is exactly what the Phase 0 AEC
  sanity test needs (`docs/BRINGUP.md` step 4): play music, record both
  channels, and confirm the music is loud on ch1 and strongly suppressed on
  ch0. Without that comparison you cannot prove AEC is working, and if AEC is
  not working, barge-in silently does not work either.
- **The 6-channel firmware is the wrong choice here.** It exposes the four raw
  microphone capsules, which invites doing DSP on the Pi — the exact thing this
  hardware exists to avoid. It also costs USB bandwidth and CPU for data we
  throw away, and it breaks `/etc/asound.conf`, which pins capture to a
  2-channel device (`hermit_dsnoop` declares `channels 2`). If
  `arecord --dump-hw-params` reports 6 channels, you flashed the wrong one.
- **A 1-channel firmware is also wrong** even though it looks tempting for a
  "we only need the processed voice" build: it gives you no reference channel,
  so you have no way to verify or debug AEC.
- **Linear, not circular.** Our mic strip is the Linear-4 (110 mm, 33 mm
  spacing). The circular firmware assumes a different array geometry and its
  beamformer and direction-of-arrival will be wrong.

Filenames follow the pattern below. **Take the exact names from the wiki** —
the version suffix moves:

```
respeaker_flex_ua-io16-lin.bin           <-- USB, 2-channel, LINEAR   ← flash this
respeaker_flex_ua-io16-cir.bin               USB, 2-channel, circular
respeaker_flex_ua-io16-6ch-lin.bin           USB, 6-channel, linear
respeaker_flex_inthost-lr16-lin-i2c.bin      I2S, linear (what ships)
```

Mnemonic: `ua` = USB audio, `io16` = 16 kHz, `lin` = linear array, `6ch` = the
six-channel build. No `6ch` and a `lin` is what you want.

---

## 2. Before you start

You can flash from the Pi itself or from a laptop; the Pi is usually easier
because everything is already there. All commands below are for the Pi
(Raspberry Pi OS Lite 64-bit).

```bash
sudo apt-get update
sudo apt-get install -y dfu-util usbutils
dfu-util --version
```

`provision.sh` installs both of these, so if you have already provisioned, skip
this.

Download the firmware onto the machine you will flash from, and keep the file
somewhere you can type the path to:

```bash
mkdir -p ~/respeaker-fw && cd ~/respeaker-fw
# fetch the USB 2-channel LINEAR .bin from the Seeed wiki link
ls -l
```

**Power.** Flashing over a marginal USB supply is how boards get bricked. Use
the official 5.1 V / 3 A Pi PSU, plug the Flex core board directly into a Pi
USB port (not through an unpowered hub), and use a data cable — a charge-only
cable will look exactly like a dead board.

---

## 3. Enter Safe Mode

Safe Mode is a recovery firmware in the XVF3800's factory partition. It always
supports USB DFU, regardless of what is in the upgrade partition. It is how you
get from I2S firmware (which does **not** support USB DFU) to USB firmware, and
it is how you recover from a bad flash.

**Procedure for the reSpeaker Flex:**

1. Power the board off completely — unplug USB *and* any separate 5 V / 12 V
   input. Not a reboot: the button is sampled at power-on.
2. Press and hold the **BOOT** button on the core board.
3. While still holding it, reconnect power (plug the USB cable back in).
4. Keep holding for ~2 seconds after power is applied, then release.
5. The LED should blink to indicate Safe Mode.

> **Confirm this against the wiki before you do it.** The button and its
> silkscreen differ across the reSpeaker family and across board revisions —
> the sibling XVF3800 USB 4-Mic Array uses the **MUTE** button for the same
> purpose and blinks a red LED. If the board has no obvious BOOT button, look
> for a labelled pad pair to bridge with tweezers while applying power. Getting
> this wrong is harmless: you simply boot normally and try again.

Verify you are in Safe Mode:

```bash
lsusb
# Expect a Seeed device with vendor id 2886. The product id differs between
# normal firmware and DFU/Safe Mode — note both, they are useful later.

dfu-util -l
# Expect one or more DFU interfaces, e.g.:
#   Found DFU: [2886:xxxx] ver=..., devnum=..., cfg=1, intf=0, path="...",
#              alt=1, name="DFU Upgrade", serial="..."
```

**Pass criterion:** `dfu-util -l` lists at least one interface, and one of them
is the **upgrade** alt setting (`alt=1`). If `dfu-util -l` prints nothing, you
are not in Safe Mode — power off fully and try again, holding the button
longer. Do not proceed.

---

## 4. Flash

```bash
sudo dfu-util -R -e -a 1 -D ~/respeaker-fw/respeaker_flex_ua-io16-lin.bin
```

What each flag does:

| Flag | Meaning |
|---|---|
| `-a 1` | alt setting 1 = the **upgrade** partition. Alt 0 is the factory/Safe Mode image — **never write to alt 0**, that is your recovery path. |
| `-D <file>` | download (host → device), i.e. write this firmware |
| `-e` | detach afterwards |
| `-R` | reset the device when finished, so it re-enumerates on the new firmware |

Confirm the alt number against `dfu-util -l` output rather than trusting `1`
blindly — if the listing shows the upgrade interface at a different alt, use
that number.

The transfer takes a few seconds. **Do not unplug anything until dfu-util
prints that it finished.** If it errors mid-write, do not panic: the factory
Safe Mode partition is untouched, so you can always re-enter Safe Mode and
retry (see Troubleshooting).

After the reset, give it five seconds and verify.

---

## 5. Verify

Run all of these. Each one catches a different failure.

```bash
# 5.1  Did it enumerate at all, and as what?
lsusb
#   Expect a Seeed (2886:xxxx) device. If the id changed from what you saw in
#   Safe Mode, that is correct and expected.

# 5.2  What did the kernel think of it?
dmesg | tail -30
#   Expect USB Audio Class lines: "new high-speed USB device", "USB Audio
#   Device", and a snd-usb-audio binding. Errors like "device descriptor read
#   /64, error -71" mean a power or cable problem, not a firmware problem.

# 5.3  Is it a PLAYBACK device?
aplay -l
#   Expect exactly one card, e.g.
#     card 0: Flex [ReSpeaker Flex], device 0: USB Audio [USB Audio]
#   WRITE DOWN the short name in square brackets — it goes into
#   /etc/asound.conf as hermit.card. Use the NAME, not the index: USB
#   enumeration order is not stable across reboots.

# 5.4  Is it a CAPTURE device?
arecord -l
#   Expect the same card listed for capture.

# 5.5  THE IMPORTANT ONE — what does it actually support?
arecord -D hw:CARD=Flex,0 --dump-hw-params -d 1 /dev/null
aplay   -D hw:CARD=Flex,0 --dump-hw-params /dev/zero
#   (substitute your card's short name for "Flex")
```

Read the `--dump-hw-params` output carefully. It is the single most
consequential piece of information in the whole bring-up:

- **CHANNELS on capture must be 2.**
  - 2 → correct, you flashed the 2-channel USB firmware.
  - 6 → you flashed the 6-channel variant. Go back to §3 and reflash.
  - 1 → wrong variant; you have no AEC reference channel. Reflash.
- **RATE** — this is the card's native sample rate. Record it. It determines
  whether the TTS provider is asked for 16 kHz or 48 kHz PCM, whether ALSA
  `plug` has to resample for librespot and mpv, and what goes in the
  `hermit.rate` line of `/etc/asound.conf`.
- **FORMAT** — normally `S16_LE`. If the card only offers `S32_LE`, update
  `hermit.format` in `/etc/asound.conf` to match.

Then a five-second real recording, speaking normally at about 50 cm:

```bash
arecord -D plughw:CARD=Flex,0 -c 2 -r 16000 -f S16_LE -d 5 /tmp/check.wav
aplay /tmp/check.wav
```

**Pass criterion:** the file is non-empty, your voice is clearly audible and
intelligible, and there is no continuous hiss, buzz or dropout.

Record the results in the table in `docs/BRINGUP.md`. Later phases depend on
these numbers and nobody wants to plug the hardware back in to re-measure.

---

## 6. Troubleshooting

### Nothing enumerates — no card in `aplay -l` / `arecord -l`

The overwhelmingly likely cause, before the flash, is simply that the board is
still on I2S firmware. That is the entire point of §1. Re-read it.

After a successful flash, work down this list:

1. **Cable.** Swap it. A charge-only USB cable is indistinguishable from a dead
   board. `dmesg | tail` says nothing at all when you plug it in? Cable.
2. **Power.** See "USB power" below.
3. **Port.** Try the other USB ports; prefer the USB 2.0 ports (the black ones)
   for audio — USB 3.0 controllers on the Pi 4 have caused enumeration quirks
   with UAC 2.0 devices.
4. **Did provisioning hide it?** `provision.sh` blacklists `snd_bcm2835` and
   sets `dtparam=audio=off` so the Flex is the only card. That should never
   hide a USB card, but confirm with `cat /proc/asound/cards` and
   `lsmod | grep snd_usb_audio`.
5. `dmesg | grep -i usb | tail -40` and read the actual error.

### Wrong firmware flashed

Symptoms and their meaning:

- `arecord --dump-hw-params` says **6 channels** → 6-channel USB variant.
- **No capture card at all, but the ESP32 works** → still on I2S firmware; the
  flash did not take.
- **Direction-of-arrival and beamforming behave oddly**, voice sounds like it
  is being tracked to the wrong place → circular (`-cir`) firmware on a linear
  array.
- **Capture works but playback through the JST amp is silent** → check the
  mixer first (`alsamixer -c <card>`, raise `PCM-1`; the XVF3800 comes up far
  too quiet on Linux and `provision.sh` fixes this), *then* suspect firmware.

Fix in every case: go back to §3, re-enter Safe Mode, flash the correct file.
There is no penalty for reflashing and no wear concern at these counts.

### Re-entering Safe Mode after a bad flash

This is why Safe Mode lives in a separate factory partition, and why you never
write to alt 0. Even a completely corrupt upgrade image cannot stop you getting
back in:

1. Unplug **all** power — USB and the separate 5 V / 12 V input if you are
   using it. Wait five seconds. The button is sampled at power-on only.
2. Hold **BOOT**, reconnect power, hold ~2 s, release.
3. `dfu-util -l` must list a DFU interface. If it does not, try holding for
   longer, then try flashing from a different host machine (a laptop rules out
   Pi-side USB power as the variable).
4. Reflash per §4.

If `dfu-util -l` never lists anything from any host, on any cable, with the
board powered from its own supply, then it is a hardware fault — stop and
contact Seeed rather than trying more firmware.

### `dfu-util` errors

| Message | Cause / fix |
|---|---|
| `No DFU capable USB device available` | Not in Safe Mode. Repeat §3. |
| `Cannot open DFU device` / permission denied | Run with `sudo`. |
| `Invalid DFU suffix signature` (a warning) | Normal for XMOS images; dfu-util continues. Not an error. |
| `wrong alt setting` / no such interface | Read the `alt=` value from `dfu-util -l` and pass that to `-a`. |
| Transfer stalls or errors partway | Power. See below. Then retry from §3. |

### USB power

The XVF3800 plus a class-D amplifier driving a speaker is a spiky load. The
Pi's *total* downstream USB budget is about **1.2 A** shared across all ports.

Symptoms of an inadequate supply: enumeration failures, `error -71` in `dmesg`,
audible clicks or crackle at volume, the Pi's undervoltage warning, and DFU
transfers that stall.

Fixes, in order:

1. Use the **official 5.1 V / 3 A Raspberry Pi PSU**. Not a phone charger.
2. Plug the Flex **directly into the Pi**, never an unpowered hub.
3. If it still misbehaves at volume, **stop using bus power**: feed the Flex
   core board from its **separate 5 V JST / 12 V terminal input**. This is the
   real fix for click-and-brownout-under-load, and it is expected to be needed
   once the speaker is driven hard in a finished enclosure.

Check for undervoltage on the Pi side at any time:

```bash
vcgencmd get_throttled     # 0x0 is what you want
```

---

## 7. When you are done

You should have, written down:

- the card **short name** from `aplay -l` (goes into `hermit.card`)
- the native **sample rate** from `--dump-hw-params` (goes into `hermit.rate`)
- the **format** and **channel count** (2, or you flashed the wrong firmware)

Put them in the results table in `docs/BRINGUP.md` and in the operator
adjustment block at the top of `hermit/deploy/asound.conf`, then continue with
`docs/BRINGUP.md` from step 2.
