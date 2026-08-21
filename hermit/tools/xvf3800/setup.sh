#!/usr/bin/env bash
# reSpeaker Flex XVF3800 control-plane setup for a hermit device.
#
# Installs the xvf tool, grants the audio group USB control of the chip,
# and applies the level tuning that hermit defaults assume (see
# docs/xvf3800.md). Run as root on the device:
#
#   sudo bash tools/xvf3800/setup.sh
set -euo pipefail
cd "$(dirname "$0")"

apt-get install -y -qq python3-usb
install -m 0755 xvf.py /usr/local/bin/xvf

# Let the audio group drive the chip without root.
cat > /etc/udev/rules.d/71-respeaker-xvf3800.rules <<'EOF'
SUBSYSTEM=="usb", ATTRS{idVendor}=="2886", ATTRS{idProduct}=="001a", MODE="0660", GROUP="audio"
EOF
udevadm control --reload-rules
udevadm trigger
sleep 1

# Level tuning: the factory AGC target (~0.0045 = -47 dBFS) is far below
# what wake models and cloud STT expect and is the root cause of the
# "device cannot hear" class of bug. Raise it to -29 dBFS and persist to
# chip flash so it survives power cycles. With this applied, hermit.toml
# keeps wake.gain and stt.sarvam_gain at 1.0; without it, raise both to 8.0.
xvf PP_AGCONOFF --values 1
xvf PP_AGCDESIREDLEVEL --values 0.036
xvf SAVE_CONFIGURATION --values 1

echo "xvf3800 setup complete:"
xvf VERSION
xvf PP_AGCDESIREDLEVEL
