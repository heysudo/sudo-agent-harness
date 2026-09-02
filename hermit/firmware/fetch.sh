#!/usr/bin/env bash
# Download the reSpeaker Flex XVF3800 USB firmware (2-channel, 16 kHz, linear
# array) and verify it against the pinned checksum in SHA256SUMS.
#
# The image is published by Seeed at github.com/respeaker/reSpeaker_Flex without
# a licence, so it is not redistributed in this repository. See
# scripts/flash_notes.md for why THIS variant and how to flash it.
#
#   hermit/firmware/fetch.sh            # -> hermit/firmware/<name>.bin
set -euo pipefail
cd "$(dirname "$0")"

FILE="respeaker_flex_usb_l16k2ch_v1.0.3.bin"
URL="https://raw.githubusercontent.com/respeaker/reSpeaker_Flex/main/xmos_firmwares/usb/${FILE}"

verify() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum -c --status SHA256SUMS
  else shasum -a 256 -c --status SHA256SUMS; fi
}

if [[ -f "$FILE" ]] && verify; then
  echo "$FILE already present and verified"
  exit 0
fi

echo "downloading $URL"
curl -fsSL --retry 3 -o "$FILE.part" "$URL"
mv "$FILE.part" "$FILE"
verify || { echo "checksum mismatch for $FILE; refusing to keep it" >&2; rm -f "$FILE"; exit 1; }
echo "ok: $FILE verified against SHA256SUMS"
