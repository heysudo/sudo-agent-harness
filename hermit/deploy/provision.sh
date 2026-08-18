#!/usr/bin/env bash
#
# HERMIT — one-shot provisioning for a fresh Raspberry Pi OS Lite 64-bit install.
#
#   Target : Raspberry Pi 4 Model B, 1 GB RAM, aarch64, headless, Ethernet.
#   Run as : root, ON THE PI.        sudo ./provision.sh
#   Safe to re-run: yes.  Every step is guarded and idempotent.  Re-running
#   after an OS update, or after editing this script, is the intended way to
#   converge the machine back to a known state.
#
# What this script does NOT do, on purpose:
#   * It does not compile anything.  The hermit binary is cross-compiled on a
#     dev machine and copied in.  There is no Rust toolchain on this Pi and
#     there never will be — see deploy/README.md.
#   * It does not flash the reSpeaker firmware.  That is a physical procedure
#     with a button press in it; see scripts/flash_notes.md.
#   * It does not write any secret.  It creates an empty 0600 template and
#     stops.  It will never overwrite an /etc/hermit/hermit.env that exists.
#   * It does not touch ssh, dhcpcd/NetworkManager, systemd-networkd, wpa
#     supplicant or avahi.  Locking yourself out of a headless box at 1am is
#     not a learning experience anyone needs.
#
set -euo pipefail


# ===========================================================================
# Constants.  Everything the script writes lives under one of these.
# ===========================================================================

readonly HERMIT_USER="hermit"
readonly HERMIT_GROUP="hermit"
readonly OPT_DIR="/opt/hermit"
readonly STATE_DIR="/var/lib/hermit"
readonly ETC_DIR="/etc/hermit"
readonly RUN_DIR="/run/hermit"
readonly ENV_FILE="${ETC_DIR}/hermit.env"

# Directory this script lives in, so we can find asound.conf and the units
# next to it regardless of where it was invoked from.
readonly DEPLOY_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

# Accumulated for the summary at the end.
CHANGES=()
MANUAL=()
REBOOT_REQUIRED=0


# ===========================================================================
# Logging helpers.  Everything the script says goes through these so the
# output is greppable and consistently prefixed.
# ===========================================================================

log()   { printf '\033[0;32m[ hermit ]\033[0m %s\n' "$*"; }
warn()  { printf '\033[0;33m[ warn   ]\033[0m %s\n' "$*" >&2; }
die()   { printf '\033[0;31m[ FATAL  ]\033[0m %s\n' "$*" >&2; exit 1; }
step()  { printf '\n\033[1;36m==> %s\033[0m\n' "$*"; }
# Record a change for the end-of-run summary.
changed() { CHANGES+=("$1"); log "$1"; }
# Record something the human still has to do.
todo()    { MANUAL+=("$1"); }

# Append $2 to file $1 only if the exact line is not already present.
# Returns 0 if it appended (i.e. something changed), 1 if it was already there.
append_once() {
    local file="$1" line="$2"
    [[ -f "$file" ]] || touch "$file"
    if grep -qxF -- "$line" "$file"; then
        return 1
    fi
    printf '%s\n' "$line" >> "$file"
    return 0
}

# Write $2 to path $1 with mode $3, but only if the content differs.  Keeps
# re-runs quiet and avoids pointlessly bumping mtimes (which would restart
# units for no reason).  Returns 0 if it wrote, 1 if already correct.
write_if_changed() {
    local path="$1" content="$2" mode="${3:-0644}"
    if [[ -f "$path" ]] && [[ "$(cat -- "$path")" == "$content" ]]; then
        chmod "$mode" "$path"
        return 1
    fi
    install -D -m "$mode" /dev/null "$path"
    printf '%s\n' "$content" > "$path"
    chmod "$mode" "$path"
    return 0
}

# Disable + stop a unit, but only if it actually exists on this system.
# Never errors out when the unit is absent (Raspberry Pi OS images vary).
disable_unit_if_present() {
    local unit="$1" why="$2"
    if systemctl list-unit-files --no-legend "$unit" 2>/dev/null | grep -q .; then
        if systemctl is-enabled --quiet "$unit" 2>/dev/null \
           || systemctl is-active --quiet "$unit" 2>/dev/null; then
            systemctl disable --now "$unit" >/dev/null 2>&1 || true
            changed "disabled ${unit} (${why})"
        fi
    fi
}


# ===========================================================================
# 0. Preflight.  Refuse to run anywhere this script would do damage.
# ===========================================================================

preflight() {
    step "Preflight checks"

    [[ ${EUID} -eq 0 ]] || die "must run as root:  sudo $0"

    local arch; arch="$(uname -m)"
    [[ "$arch" == "aarch64" ]] || die \
        "architecture is '${arch}', expected aarch64. This is the 64-bit Raspberry Pi OS Lite build only. \
A 32-bit (armhf) install will not run the cross-compiled binary — reflash the Pi with the 64-bit image."

    # /proc/device-tree/model is NUL-terminated; tr strips it.
    local model=""
    [[ -r /proc/device-tree/model ]] && model="$(tr -d '\0' < /proc/device-tree/model)" || true
    [[ "$model" == *"Raspberry Pi"* ]] || die \
        "this does not look like a Raspberry Pi (model='${model:-unknown}'). Refusing to run — \
this script edits /boot/firmware/config.txt and would corrupt a non-Pi system."
    log "model: ${model}"
    case "$model" in
        *"Raspberry Pi 4"*) : ;;
        *) warn "expected a Raspberry Pi 4 Model B; found '${model}'. Continuing, but thermal and \
USB-power guidance in docs/BRINGUP.md assumes a Pi 4." ;;
    esac

    # Raspberry Pi OS identifies as Debian.  Accept both ID and ID_LIKE.
    if [[ -r /etc/os-release ]]; then
        # shellcheck disable=SC1091
        . /etc/os-release
        log "os: ${PRETTY_NAME:-unknown}"
        [[ "${ID:-}" == "debian" || "${ID_LIKE:-}" == *debian* ]] || die \
            "expected Debian-based Raspberry Pi OS, found ID='${ID:-?}'"
    else
        die "/etc/os-release missing — cannot identify the OS"
    fi

    # Locate the boot partition.  Bookworm and later use /boot/firmware.
    if   [[ -f /boot/firmware/config.txt ]]; then BOOT_DIR=/boot/firmware
    elif [[ -f /boot/config.txt          ]]; then BOOT_DIR=/boot
    else die "cannot find config.txt in /boot/firmware or /boot"
    fi
    readonly BOOT_DIR
    log "boot config: ${BOOT_DIR}/config.txt"

    # The deploy files we are going to install must actually be next to us.
    [[ -f "${DEPLOY_DIR}/asound.conf"   ]] || die "missing ${DEPLOY_DIR}/asound.conf"
    [[ -f "${DEPLOY_DIR}/hermit.service" ]] || die "missing ${DEPLOY_DIR}/hermit.service"

    log "preflight OK"
}


# ===========================================================================
# 1. Service account.
#     System user, no login shell, no password, home = the state dir.
#     Membership of `audio` is what grants access to /dev/snd.
# ===========================================================================

create_user() {
    step "Service account"

    if getent group "$HERMIT_GROUP" >/dev/null; then
        log "group ${HERMIT_GROUP} already exists"
    else
        groupadd --system "$HERMIT_GROUP"
        changed "created system group ${HERMIT_GROUP}"
    fi

    if id -u "$HERMIT_USER" >/dev/null 2>&1; then
        log "user ${HERMIT_USER} already exists"
    else
        useradd --system \
                --gid "$HERMIT_GROUP" \
                --home-dir "$STATE_DIR" \
                --no-create-home \
                --shell /usr/sbin/nologin \
                --comment "HERMIT voice agent daemon" \
                "$HERMIT_USER"
        changed "created system user ${HERMIT_USER}"
    fi

    # `audio` gives /dev/snd access; without it every ALSA open returns EACCES.
    if id -nG "$HERMIT_USER" | tr ' ' '\n' | grep -qx audio; then
        log "user ${HERMIT_USER} already in group audio"
    else
        usermod -aG audio "$HERMIT_USER"
        changed "added ${HERMIT_USER} to group audio"
    fi
}


# ===========================================================================
# 2. Filesystem layout.
#
#     /opt/hermit/bin      the binary (root-owned, hermit only needs to read)
#     /opt/hermit/config   hermit.toml, prompts/, skills/, identity.md, core.md,
#                          stations.toml — all read-only to the daemon
#     /var/lib/hermit      SQLite db + runtime state, hermit-owned, read-write
#     /etc/hermit          secrets, 0750, hermit-readable
#     /run/hermit          sockets (mpv ipc), recreated at boot by systemd
# ===========================================================================

create_dirs() {
    step "Filesystem layout"

    # Code and config: owned by root so a compromised daemon cannot rewrite
    # its own prompts or replace its own binary.  Group-readable by hermit.
    install -d -o root -g "$HERMIT_GROUP" -m 0755 "$OPT_DIR"
    install -d -o root -g "$HERMIT_GROUP" -m 0755 "${OPT_DIR}/bin"
    install -d -o root -g "$HERMIT_GROUP" -m 0755 "${OPT_DIR}/config"

    # Mutable state: owned by hermit.
    install -d -o "$HERMIT_USER" -g "$HERMIT_GROUP" -m 0750 "$STATE_DIR"

    # Secrets directory: not world-readable.
    install -d -o root -g "$HERMIT_GROUP" -m 0750 "$ETC_DIR"

    # Runtime dir.  systemd's RuntimeDirectory= recreates this on every start;
    # we create it now so a manual foreground run before the first `systemctl
    # start` also works.
    install -d -o "$HERMIT_USER" -g "$HERMIT_GROUP" -m 0770 "$RUN_DIR"

    # tmpfiles.d makes /run/hermit reappear after a reboot even if the daemon
    # is not enabled yet (e.g. mpv sidecar starting first).
    write_if_changed /etc/tmpfiles.d/hermit.conf \
"# HERMIT runtime directory (sockets: mpv ipc).  /run is a tmpfs, so this must
# be recreated on every boot.
d ${RUN_DIR} 0770 ${HERMIT_USER} ${HERMIT_GROUP} -" 0644 \
        && changed "wrote /etc/tmpfiles.d/hermit.conf" || true
    systemd-tmpfiles --create /etc/tmpfiles.d/hermit.conf >/dev/null 2>&1 || true

    log "layout: ${OPT_DIR}/{bin,config}  ${STATE_DIR}  ${ETC_DIR}  ${RUN_DIR}"
}


# ===========================================================================
# 3. Packages.
#
#     Deliberately small.  This box runs one binary and two sidecars; every
#     extra package is more SD wear, more attack surface and more to update.
#     NOTHING here is a build dependency — no gcc, no rustc, no pkg-config.
# ===========================================================================

install_packages() {
    step "Packages"

    export DEBIAN_FRONTEND=noninteractive

    # Refresh the index at most once a day so re-running the script is fast.
    local stamp=/var/lib/apt/periodic/hermit-update-stamp
    if [[ ! -f "$stamp" ]] || [[ $(( $(date +%s) - $(stat -c %Y "$stamp") )) -gt 86400 ]]; then
        log "apt-get update ..."
        apt-get update -qq
        install -D /dev/null "$stamp"
    else
        log "apt index is fresh, skipping update"
    fi

    # Core set.
    #   alsa-utils : aplay/arecord/amixer/alsactl/speaker-test — bring-up AND
    #                the alsactl store/restore that persists mixer levels.
    #   mpv        : internet radio sidecar, driven over a JSON IPC socket.
    #   dfu-util   : reSpeaker firmware flashing (scripts/flash_notes.md).
    #   sqlite3    : inspecting the daemon's database by hand.
    #   curl       : bring-up latency measurement, health checks.
    #   ca-certificates : TLS to Cerebras/Deepgram/Cartesia/etc.
    #   zram-tools : compressed swap on images without Raspberry Pi OS' native
    #                rpi-swap zram generator; see configure_swap().
    #   usbutils   : lsusb, for confirming the Flex enumerated.
    local pkgs=(
        alsa-utils
        mpv
        dfu-util
        sqlite3
        curl
        ca-certificates
        usbutils
    )

    # Current Raspberry Pi OS images ship rpi-swap, backed by systemd's zram
    # generator. Installing zram-tools beside it creates two owners for /dev/zram0;
    # zramswap then fails every boot with EBUSY even though native swap is healthy.
    if [[ -f /etc/rpi/swap.conf ]] && [[ -x /usr/lib/systemd/system-generators/zram-generator ]]; then
        log "native rpi-swap zram detected; not installing duplicate zram-tools"
    else
        pkgs+=(zram-tools)
    fi

    # Time sync: prefer whatever the image already ships.  systemd-timesyncd is
    # present on Raspberry Pi OS Lite; only fall back to chrony if it is not.
    if systemctl list-unit-files --no-legend systemd-timesyncd.service 2>/dev/null | grep -q .; then
        log "time sync: systemd-timesyncd is available"
    else
        pkgs+=(chrony)
        log "time sync: systemd-timesyncd absent, will install chrony"
    fi

    local missing=()
    local p
    for p in "${pkgs[@]}"; do
        dpkg-query -W -f='${Status}' "$p" 2>/dev/null | grep -q "ok installed" || missing+=("$p")
    done

    if [[ ${#missing[@]} -gt 0 ]]; then
        log "installing: ${missing[*]}"
        apt-get install -y -qq --no-install-recommends "${missing[@]}"
        changed "installed packages: ${missing[*]}"
    else
        log "all required packages already installed"
    fi

    install_librespot
}

# ---------------------------------------------------------------------------
# librespot: the Spotify Connect endpoint.
#
# It is NOT in the Raspberry Pi OS / Debian bookworm archive.  If a future
# image does carry it we take the packaged one; otherwise we leave a precise
# manual instruction rather than silently curl|sh-ing a binary from the
# internet as root, which is not a thing this script is going to do.
# ---------------------------------------------------------------------------
install_librespot() {
    if command -v librespot >/dev/null 2>&1; then
        log "librespot present at $(command -v librespot)"
        return
    fi

    if apt-cache show librespot >/dev/null 2>&1; then
        log "librespot is packaged on this release, installing from apt"
        apt-get install -y -qq --no-install-recommends librespot
        changed "installed librespot from apt"
        return
    fi

    warn "librespot is not packaged for this release and is not installed."
    todo "Install librespot manually (Spotify playback will not work until you do):
       1. On your DEV machine, download the aarch64 release tarball from
            https://github.com/librespot-org/librespot/releases
          Pick the asset for  aarch64-unknown-linux-gnu  (NOT armhf, NOT musl-arm).
       2. Verify the checksum published on that release page.
       3. Copy it over and install:
            scp librespot-*.tar.gz pi@<pi>:/tmp/
            ssh pi@<pi>
            sudo tar -xzf /tmp/librespot-*.tar.gz -C /usr/local/bin librespot
            sudo chmod 0755 /usr/local/bin/librespot
            librespot --version
       4. Re-run provision.sh — it will pick librespot up and enable the unit."
}


# ===========================================================================
# 4. Memory: zram swap + swappiness.
#
#     1 GB of RAM, a daemon with LTO'd release code, plus mpv and librespot.
#     Real swap on an SD card is both slow and destructive, so: 512 MB of
#     zstd-compressed swap in RAM (typically 2.5-3x compression, so ~512 MB of
#     swap costs ~180 MB of real RAM), and swappiness cranked DOWN to 10 so the
#     kernel only reaches for it under genuine pressure rather than paging out
#     the audio thread's working set during a quiet minute.
#
#     dphys-swapfile (the SD-card swap file Raspberry Pi OS ships) is disabled.
# ===========================================================================

configure_swap() {
    step "Memory: zram swap and swappiness"

    disable_unit_if_present dphys-swapfile.service "SD-card swap file: slow, and it wears the card out"

    # Raspberry Pi OS Trixie owns /dev/zram0 through rpi-swap + zram-generator.
    # Never race that with zram-tools: the loser fails boot with EBUSY and leaves
    # systemd degraded even though the native swap device is already active.
    if [[ -f /etc/rpi/swap.conf ]] && [[ -x /usr/lib/systemd/system-generators/zram-generator ]]; then
        disable_unit_if_present zramswap.service "native rpi-swap already owns /dev/zram0"
        log "using native rpi-swap zram configuration"
    # Older Bookworm-style images use zram-tools and /etc/default/zramswap.
    elif [[ -d /etc/default ]] && dpkg-query -W -f='${Status}' zram-tools 2>/dev/null | grep -q "ok installed"; then
        if write_if_changed /etc/default/zramswap \
"# Managed by HERMIT provision.sh
# 512 MB of compressed swap held in RAM.  zstd gives the best ratio per CPU
# cycle on the Pi 4's Cortex-A72.  PRIORITY above any disk swap so the kernel
# always prefers zram.
ALGO=zstd
SIZE=512
PRIORITY=100" 0644
        then
            changed "configured 512 MB zstd zram swap (/etc/default/zramswap)"
            systemctl enable zramswap.service >/dev/null 2>&1 || true
            systemctl restart zramswap.service >/dev/null 2>&1 \
                || warn "could not restart zramswap.service; it will come up on reboot"
        else
            log "zram swap already configured"
            systemctl enable zramswap.service >/dev/null 2>&1 || true
        fi
    else
        warn "zram-tools not installed; skipping zram swap configuration"
    fi

    # vm.swappiness — persistent, and applied now.
    if write_if_changed /etc/sysctl.d/99-hermit.conf \
"# Managed by HERMIT provision.sh
#
# Only swap under real pressure.  The daemon holds an audio ring buffer and a
# wake-word model resident; paging either out shows up directly as latency.
vm.swappiness = 10

# Write back dirty pages sooner and in smaller batches.  On an SD card a big
# writeback burst can stall the whole system for hundreds of milliseconds,
# which is audible as a dropout.
vm.dirty_background_ratio = 5
vm.dirty_ratio = 10" 0644
    then
        changed "wrote /etc/sysctl.d/99-hermit.conf (swappiness=10, dirty ratios)"
    else
        log "sysctl tuning already in place"
    fi
    sysctl --quiet --system >/dev/null 2>&1 || warn "sysctl --system reported an error"
}


# ===========================================================================
# 5. Boot configuration: kill the onboard audio, HDMI audio and Bluetooth.
#
#     The reSpeaker Flex must be the ONLY sound card.  If snd_bcm2835 (the
#     3.5 mm jack) or vc4-hdmi register cards, they compete for card index 0,
#     which makes `hw:0` mean different things on different boots and can put
#     the Flex behind them in enumeration order.  Removing them entirely is
#     both a correctness fix and a small RAM/CPU saving.
#
#     Bluetooth is off because nothing uses it and its firmware upload +
#     hciuart service is pure startup cost and RF noise.
#
#     Everything is written inside a marked block so re-runs are idempotent
#     and a human can see exactly what we touched.
# ===========================================================================

configure_boot() {
    step "Boot configuration (${BOOT_DIR}/config.txt)"

    local cfg="${BOOT_DIR}/config.txt"
    local marker="# ---- HERMIT (managed by provision.sh) ----"

    # Back up once, the first time we ever touch the file.
    if [[ ! -f "${cfg}.hermit-orig" ]]; then
        cp -a "$cfg" "${cfg}.hermit-orig"
        changed "backed up ${cfg} to ${cfg}.hermit-orig"
    fi

    # The stock image ships `dtoverlay=vc4-kms-v3d`.  Adding a SECOND
    # dtoverlay line would load the overlay twice, so instead we append the
    # `noaudio` parameter to the existing line in place.  That removes the
    # vc4-hdmi ALSA cards while leaving the display driver itself alone.
    if grep -qE '^\s*dtoverlay=vc4-kms-v3d' "$cfg"; then
        if ! grep -qE '^\s*dtoverlay=vc4-kms-v3d.*noaudio' "$cfg"; then
            sed -i -E 's/^(\s*dtoverlay=vc4-kms-v3d[^[:space:]]*)$/\1,noaudio/' "$cfg"
            changed "added ,noaudio to the vc4-kms-v3d overlay (removes HDMI audio cards)"
            REBOOT_REQUIRED=1
        else
            log "vc4-kms-v3d already has noaudio"
        fi
    fi

    # Our own block, appended once.  Later settings in config.txt win over
    # earlier ones for dtparam, so appending is safe even if the stock image
    # already set dtparam=audio=on further up.
    if ! grep -qF "$marker" "$cfg"; then
        cat >> "$cfg" <<EOF

${marker}
# The reSpeaker Flex XVF3800 (USB) is the one and only sound card.  See
# hermit/deploy/asound.conf for why the topology is locked that way.

# Disable the onboard bcm2835 audio (3.5 mm jack + HDMI): we must never use it,
# and it otherwise steals a low card index from the USB device.
dtparam=audio=off

# Headless: no Bluetooth stack, no rainbow splash, no boot delay hunting for
# devices that are not there.
dtoverlay=disable-bt
disable_splash=1
boot_delay=0
EOF
        changed "appended HERMIT block to ${cfg} (audio=off, disable-bt, no splash)"
        REBOOT_REQUIRED=1
    else
        log "HERMIT block already present in ${cfg}"
    fi

    # Belt and braces: even if an overlay change is missed, do not load the
    # onboard sound module at all.
    if write_if_changed /etc/modprobe.d/hermit-no-onboard-audio.conf \
"# Managed by HERMIT provision.sh
# The Pi's onboard audio must never register an ALSA card: all playback and
# capture go through the reSpeaker Flex so the XVF3800's hardware AEC always
# has a loopback reference.  Without that, barge-in does not work.
blacklist snd_bcm2835" 0644
    then
        changed "blacklisted snd_bcm2835"
        REBOOT_REQUIRED=1
    else
        log "snd_bcm2835 already blacklisted"
    fi

    # Kernel command line: stop USB autosuspend.  A suspended USB audio device
    # produces exactly the symptom you least want to debug — the first second
    # of the first response after a quiet period is clipped.
    local cmdline="${BOOT_DIR}/cmdline.txt"
    if [[ -f "$cmdline" ]]; then
        if ! grep -q 'usbcore.autosuspend=-1' "$cmdline"; then
            [[ -f "${cmdline}.hermit-orig" ]] || cp -a "$cmdline" "${cmdline}.hermit-orig"
            # cmdline.txt MUST stay a single line: edit in place, no newline.
            sed -i 's/[[:space:]]*$//' "$cmdline"
            sed -i '1s|$| usbcore.autosuspend=-1|' "$cmdline"
            changed "disabled USB autosuspend in ${cmdline}"
            REBOOT_REQUIRED=1
        else
            log "USB autosuspend already disabled"
        fi
    fi
}


# ===========================================================================
# 6. Trim services.
#
#     EXPLICIT list only.  We never disable ssh, dhcpcd, NetworkManager,
#     systemd-networkd, wpa_supplicant or avahi-daemon:
#       * ssh is the only way into a headless box;
#       * the network ones are obvious;
#       * avahi is REQUIRED — librespot's Spotify Connect discovery and
#         (optional) shairport-sync AirPlay both advertise over mDNS.
# ===========================================================================

trim_services() {
    step "Disabling services we do not need"

    disable_unit_if_present bluetooth.service   "Bluetooth is not used in this topology"
    disable_unit_if_present hciuart.service     "Bluetooth HCI over UART, disabled with the radio"
    disable_unit_if_present triggerhappy.service "GPIO hotkey daemon, nothing to trigger on a headless box"
    disable_unit_if_present triggerhappy.socket  "GPIO hotkey daemon socket"
    disable_unit_if_present ModemManager.service "no cellular modem"

    # rsyslog duplicates the journal straight onto the SD card.  The journal is
    # made volatile below, so keeping rsyslog would defeat the whole point.
    disable_unit_if_present rsyslog.service     "duplicates the journal to the SD card"

    # Unattended apt work fires at unpredictable times and pegs the CPU for
    # minutes on a 4-core Pi, which is heard as stutter mid-conversation.
    # Updates become a deliberate, operator-run action instead.
    disable_unit_if_present apt-daily.timer         "unscheduled CPU spikes during conversations"
    disable_unit_if_present apt-daily-upgrade.timer "unscheduled CPU spikes during conversations"
    disable_unit_if_present man-db.timer            "rebuilds man page index, pure SD wear"

    # Explicitly ensure the things we must NOT have broken are still enabled.
    local must_keep=(ssh.service)
    local u
    for u in "${must_keep[@]}"; do
        if systemctl list-unit-files --no-legend "$u" 2>/dev/null | grep -q .; then
            systemctl is-enabled --quiet "$u" 2>/dev/null \
                || warn "${u} is NOT enabled — you may lose remote access on reboot"
        fi
    done
    log "ssh / networking / avahi left untouched"
}


# ===========================================================================
# 7. CPU governor = performance, persistently.
#
#     The default `ondemand` governor ramps clocks up only after it observes
#     load.  Our load arrives as a 20 ms burst (wake word fires, audio frame
#     must be encoded and posted) and is over before the governor reacts, so
#     the burst runs at 600 MHz instead of 1.5 GHz.  That is tens of
#     milliseconds of avoidable latency on the exact path we care about.
#
#     Costs: a few hundred mW and a few degrees.  That is why the enclosure
#     needs a heatsink and vents — see docs/BRINGUP.md step 6.
#
#     Implemented as our own tiny unit rather than cpufrequtils so there is
#     one fewer package and the behaviour is visible in `systemctl status`.
# ===========================================================================

configure_governor() {
    step "CPU governor"

    if [[ ! -d /sys/devices/system/cpu/cpu0/cpufreq ]]; then
        warn "no cpufreq sysfs interface; skipping governor configuration"
        return
    fi

    if write_if_changed /usr/local/sbin/hermit-set-governor \
'#!/bin/sh
# Managed by HERMIT provision.sh — pin every CPU to the performance governor.
set -eu
for g in /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor; do
    [ -w "$g" ] || continue
    echo performance > "$g" || true
done
exit 0' 0755
    then
        changed "installed /usr/local/sbin/hermit-set-governor"
    fi

    if write_if_changed /etc/systemd/system/hermit-cpu-governor.service \
"[Unit]
Description=HERMIT: pin CPU scaling governor to performance
Documentation=file://${DEPLOY_DIR}/provision.sh
DefaultDependencies=no
After=sysinit.target
Before=hermit.service

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/local/sbin/hermit-set-governor

[Install]
WantedBy=multi-user.target" 0644
    then
        changed "installed hermit-cpu-governor.service"
    fi

    systemctl daemon-reload
    systemctl enable --now hermit-cpu-governor.service >/dev/null 2>&1 \
        || warn "could not start hermit-cpu-governor.service"

    local cur=""
    [[ -r /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor ]] \
        && cur="$(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor)" || true
    log "cpu0 governor is now: ${cur:-unknown}"
}


# ===========================================================================
# 8. Time sync.
#
#     Non-negotiable: every API here (Cerebras, Deepgram, Cartesia, Spotify)
#     uses TLS, and TLS certificate validation fails outright if the clock is
#     wrong.  A Pi has no RTC, so it boots in 1970 until NTP lands.
# ===========================================================================

configure_time() {
    step "Time synchronisation"

    if systemctl list-unit-files --no-legend systemd-timesyncd.service 2>/dev/null | grep -q .; then
        systemctl enable --now systemd-timesyncd.service >/dev/null 2>&1 || true
        timedatectl set-ntp true >/dev/null 2>&1 || true
        changed "enabled systemd-timesyncd"
    elif systemctl list-unit-files --no-legend chrony.service 2>/dev/null | grep -q .; then
        systemctl enable --now chrony.service >/dev/null 2>&1 || true
        changed "enabled chrony"
    else
        warn "no NTP client found — TLS will fail until the clock is correct"
    fi

    timedatectl status 2>/dev/null | sed 's/^/          /' || true
}


# ===========================================================================
# 9. Journald: volatile with hard caps.
#
#     The single biggest source of SD-card writes on an always-on Pi is the
#     journal.  Storage=volatile keeps it in /run (tmpfs), capped so it can
#     never eat the RAM we do not have.  Logs do not survive a reboot — that
#     is the trade, and it is the right one here: anything worth keeping goes
#     into the daemon's SQLite database or is shipped off-box.
#
#     `journalctl -u hermit -f` still works exactly as usual while running.
# ===========================================================================

configure_journald() {
    step "journald"

    if write_if_changed /etc/systemd/journald.conf.d/10-hermit.conf \
"# Managed by HERMIT provision.sh
[Journal]
# RAM only: no journal writes to the SD card at all.
Storage=volatile
# Hard caps so the journal can never squeeze the daemon on a 1 GB box.
RuntimeMaxUse=32M
RuntimeMaxFileSize=8M
RuntimeKeepFree=64M
# Nothing is listening on syslog and rsyslog is disabled; don't pay for it.
ForwardToSyslog=no
ForwardToWall=no
# A crash loop must not be able to fill the ring in seconds.
RateLimitIntervalSec=30s
RateLimitBurst=1000" 0644
    then
        changed "made the journal volatile and capped (32M in tmpfs)"
        systemctl restart systemd-journald >/dev/null 2>&1 || true
    else
        log "journald already configured"
    fi

    # Remove any persistent journal left over from before this ran.
    if [[ -d /var/log/journal ]]; then
        rm -rf /var/log/journal
        changed "removed the persistent journal at /var/log/journal"
    fi
}


# ===========================================================================
# 10. ALSA: install /etc/asound.conf and set the mixer.
# ===========================================================================

configure_alsa() {
    step "ALSA"

    # --- install the config -------------------------------------------------
    if ! cmp -s "${DEPLOY_DIR}/asound.conf" /etc/asound.conf; then
        [[ -f /etc/asound.conf && ! -f /etc/asound.conf.hermit-orig ]] \
            && cp -a /etc/asound.conf /etc/asound.conf.hermit-orig || true
        install -m 0644 -o root -g root "${DEPLOY_DIR}/asound.conf" /etc/asound.conf
        changed "installed /etc/asound.conf"
    else
        log "/etc/asound.conf already up to date"
    fi

    # --- is the card actually here? ----------------------------------------
    # Provisioning is often run BEFORE the firmware is flashed.  That is fine:
    # everything above is card-independent.  We just cannot set a mixer level
    # on a card that is not enumerating, so we say so clearly and move on.
    if ! aplay -l 2>/dev/null | grep -q '^card '; then
        warn "no ALSA playback card is present."
        warn "This is EXPECTED if you have not flashed the reSpeaker USB firmware yet."
        todo "Flash the reSpeaker Flex USB 2-channel firmware (see hermit/scripts/flash_notes.md),
       then re-run this script so the mixer levels can be set and stored."
        return
    fi

    log "cards present:"
    aplay -l 2>/dev/null | grep '^card ' | sed 's/^/          /' || true

    # --- check /etc/asound.conf points at a real card ----------------------
    local conf_card
    conf_card="$(grep -E '^hermit\.card' /etc/asound.conf | head -1 | sed -E 's/.*"(.*)".*/\1/')"
    if ! aplay -l 2>/dev/null | grep -qE "^card [0-9]+: ${conf_card} \["; then
        warn "/etc/asound.conf has hermit.card \"${conf_card}\" but no such card is listed above."
        todo "Edit the OPERATOR ADJUSTMENT BLOCK at the top of /etc/asound.conf: set
       hermit.card to the short name shown in square brackets by 'aplay -l',
       and hermit.rate to the native rate from '--dump-hw-params'.  Then
       re-run this script.  (Also update hermit/deploy/asound.conf in the repo
       so the next flash of this build gets it right.)"
        return
    fi
    log "asound.conf card reference '${conf_card}' matches a present card"

    configure_mixer "$conf_card"
}

# ---------------------------------------------------------------------------
# The XVF3800 comes up far too quiet on Linux: its PCM playback control
# defaults to a low value and there is no desktop mixer here to fix it.  Push
# the HARDWARE control to maximum and let the softvol ceiling in asound.conf
# (max_dB -2.5) be the thing that protects the 3 W speaker.  Hardware at max +
# a software ceiling is the correct arrangement: one well-defined limit,
# applied in one place, with full DAC range underneath it.
#
# Control naming varies across firmware revisions ("PCM-1", "PCM", "Speaker"),
# so probe for whichever exists rather than guessing.
# ---------------------------------------------------------------------------
configure_mixer() {
    local card="$1"
    local touched=0 ctl

    log "setting mixer levels on card '${card}'"

    # Hardware playback controls.
    for ctl in "PCM-1" "PCM" "Speaker" "Master" "Headphone"; do
        if amixer -c "$card" sget "$ctl" >/dev/null 2>&1; then
            amixer -c "$card" -q sset "$ctl" 100% unmute >/dev/null 2>&1 || true
            log "  playback control '${ctl}' -> 100% (ceiling is enforced by softvol)"
            touched=1
        fi
    done

    # Capture controls: the XVF3800 has already applied AGC on-chip, so the
    # host-side capture gain just needs to be at unity and unmuted.
    for ctl in "Mic" "Capture" "PCM Capture Source"; do
        if amixer -c "$card" sget "$ctl" >/dev/null 2>&1; then
            amixer -c "$card" -q sset "$ctl" 100% cap >/dev/null 2>&1 || true
            log "  capture control '${ctl}' -> 100%"
            touched=1
        fi
    done

    [[ $touched -eq 1 ]] || warn "no recognised mixer controls found on card '${card}' — \
run 'alsamixer -c ${card}' and set the playback level by hand, then 'alsactl store'"

    # The softvol control ("Hermit Master") does not exist until the softvol
    # PCM has been opened at least once.  Open it briefly on silence to create
    # the control, then set it to maximum — which, because of max_dB, is the
    # protected ceiling and not full scale.
    local rate
    # Strip the trailing comment BEFORE pulling the number out, otherwise the
    # explanatory "48000, or 16000 if ..." in the adjustment block comes along
    # for the ride and `rate` ends up multi-line.
    rate="$(grep -E '^hermit\.rate' /etc/asound.conf | head -1 | sed -E 's/#.*//' \
            | grep -oE '[0-9]+' | head -1)"
    [[ -n "$rate" ]] || rate=48000
    if timeout 10 aplay -D hermit_out -f S16_LE -c 2 -r "$rate" -d 1 /dev/zero >/dev/null 2>&1; then
        if amixer -c "$card" sget "Hermit Master" >/dev/null 2>&1; then
            amixer -c "$card" -q sset "Hermit Master" 100% >/dev/null 2>&1 || true
            log "  softvol control 'Hermit Master' -> 100% (= -2.5 dBFS ceiling)"
        fi
    else
        warn "could not open the 'hermit_out' PCM to create the softvol control."
        warn "Check the card name and rate in the adjustment block of /etc/asound.conf."
    fi

    # Persist.  alsa-restore.service replays this at every boot.
    if alsactl store >/dev/null 2>&1; then
        changed "stored mixer levels (alsactl store -> /var/lib/alsa/asound.state)"
    else
        warn "alsactl store failed; mixer levels will not survive a reboot"
    fi
    systemctl enable alsa-restore.service >/dev/null 2>&1 || true
}


# ===========================================================================
# 11. Secrets template.
#
#     NEVER overwrites an existing file.  NEVER contains a real value.  The
#     repo must never contain this file at all.
# ===========================================================================

create_env_template() {
    step "Secrets template"

    if [[ -f "$ENV_FILE" ]]; then
        # Do not touch the contents, but do make sure the permissions have not
        # drifted — a world-readable env file is the whole ballgame.
        chown "$HERMIT_USER":"$HERMIT_GROUP" "$ENV_FILE"
        chmod 0600 "$ENV_FILE"
        log "${ENV_FILE} already exists — left untouched (mode corrected to 0600 ${HERMIT_USER}:${HERMIT_GROUP})"
        # Warn about any key that is still empty.
        local key missing=()
        for key in CEREBRAS_API_KEY PARALLEL_API_KEY FIRECRAWL_API_KEY CARTESIA_API_KEY \
                   ELEVENLABS_API_KEY DEEPGRAM_API_KEY PICOVOICE_ACCESS_KEY \
                   SPOTIFY_CLIENT_ID SPOTIFY_CLIENT_SECRET SPOTIFY_REFRESH_TOKEN; do
            grep -qE "^${key}=.+" "$ENV_FILE" || missing+=("$key")
        done
        if [[ ${#missing[@]} -gt 0 ]]; then
            warn "these keys are still empty in ${ENV_FILE}: ${missing[*]}"
            todo "Fill in the empty keys in ${ENV_FILE}: ${missing[*]}"
        fi
        return
    fi

    umask 077
    cat > "$ENV_FILE" <<'EOF'
# HERMIT secrets.  Loaded by systemd as EnvironmentFile= for hermit.service
# and hermit-consolidate.service.
#
#   * This file MUST NOT be committed to the repo, copied into /opt/hermit,
#     or included in any backup that leaves the device.
#   * Mode 0600, owned hermit:hermit.  provision.sh re-asserts that every run.
#     systemd reads EnvironmentFile= as root before dropping to User=hermit,
#     so 0600 is readable by the unit and by nobody else.
#   * Format is KEY=value, one per line, NO quotes, NO `export`, no spaces
#     around the '='.  systemd is not a shell: quotes end up in the value.
#   * After editing:  sudo systemctl restart hermit
#
# ---- LLM -------------------------------------------------------------------
# Cerebras, OpenAI-compatible endpoint at api.cerebras.ai, model gpt-oss-120b.
CEREBRAS_API_KEY=

# ---- Research / web --------------------------------------------------------
PARALLEL_API_KEY=
FIRECRAWL_API_KEY=

# ---- Text to speech --------------------------------------------------------
CARTESIA_API_KEY=
ELEVENLABS_API_KEY=

# ---- Speech to text --------------------------------------------------------
DEEPGRAM_API_KEY=

# ---- Wake word -------------------------------------------------------------
# Picovoice Porcupine. The access key is tied to your Picovoice account.
PICOVOICE_ACCESS_KEY=

# ---- Spotify ---------------------------------------------------------------
# Client id/secret from the Spotify developer dashboard; the refresh token is
# obtained once via the authorization-code flow on your dev machine and pasted
# here.  librespot handles playback; these credentials are for the Web API
# calls the agent makes (search, queue, transfer playback).
SPOTIFY_CLIENT_ID=
SPOTIFY_CLIENT_SECRET=
SPOTIFY_REFRESH_TOKEN=
EOF
    chown "$HERMIT_USER":"$HERMIT_GROUP" "$ENV_FILE"
    chmod 0600 "$ENV_FILE"
    umask 022

    changed "created ${ENV_FILE} template (0600 ${HERMIT_USER}:${HERMIT_GROUP}, all values empty)"
    todo "Fill in ${ENV_FILE} with real API keys.  It is empty right now, so the
       daemon will fail its config check on first start."
}


# ===========================================================================
# 12. Sidecars: librespot (Spotify Connect) and mpv (internet radio).
#
#     Both are written here rather than shipped as repo files, because both
#     are pure deployment artifacts with no content worth version-controlling
#     beyond this script.
#
#     Both play to ALSA `default`, which asound.conf defines as the
#     volume-limited mono chain through dmix on the Flex.  That is the whole
#     point of the locked topology: music goes out of the same speaker the
#     XVF3800 is cancelling, so you can say the wake word over the top of it.
# ===========================================================================

install_sidecars() {
    step "Sidecar units (librespot, mpv)"

    # --- mpv: internet radio, controlled over a JSON IPC socket -------------
    if write_if_changed /etc/systemd/system/hermit-mpv.service \
"[Unit]
Description=HERMIT: mpv internet-radio sidecar
Documentation=file://${OPT_DIR}/config/stations.toml
After=network-online.target sound.target
Wants=network-online.target
PartOf=hermit.service

[Service]
Type=simple
User=${HERMIT_USER}
Group=${HERMIT_GROUP}
SupplementaryGroups=audio

# Shared with hermit.service.  Preserve= stops one unit stopping from deleting
# the directory (and the socket) out from under the other.
RuntimeDirectory=hermit
RuntimeDirectoryMode=0770
RuntimeDirectoryPreserve=yes

# ProtectHome + ProtectSystem=strict below leave mpv with nowhere writable
# except /run/hermit. Point HOME and the XDG caches there so mpv never trips
# over a read-only path while trying to write a state file nobody reads.
Environment=HOME=${RUN_DIR}
Environment=XDG_CACHE_HOME=${RUN_DIR}
Environment=XDG_CONFIG_HOME=${RUN_DIR}

# --idle keeps mpv alive with no file loaded, waiting for IPC commands.
# --input-ipc-server is the socket the daemon writes JSON commands to.
# --audio-device=alsa/default lands on the chain in /etc/asound.conf.
ExecStart=/usr/bin/mpv \\
    --no-video \\
    --idle=yes \\
    --no-terminal \\
    --really-quiet \\
    --input-ipc-server=${RUN_DIR}/mpv.sock \\
    --audio-device=alsa/default \\
    --audio-channels=stereo \\
    --volume=100 \\
    --cache=yes \\
    --cache-secs=10 \\
    --network-timeout=10

Restart=always
RestartSec=2

# Hardening.  /dev/snd must stay reachable, so PrivateDevices stays off and we
# allow the ALSA device class explicitly.
NoNewPrivileges=yes
PrivateTmp=yes
ProtectHome=yes
ProtectSystem=strict
ReadWritePaths=${RUN_DIR}
PrivateDevices=no
DevicePolicy=closed
DeviceAllow=char-alsa rw

# mpv decoding a stream is small; cap it so a pathological stream cannot
# push the daemon into swap on a 1 GB box.
MemoryMax=128M

[Install]
WantedBy=multi-user.target" 0644
    then
        changed "installed hermit-mpv.service"
    else
        log "hermit-mpv.service already up to date"
    fi

    # --- librespot: Spotify Connect endpoint --------------------------------
    if command -v librespot >/dev/null 2>&1; then
        local librespot_bin; librespot_bin="$(command -v librespot)"
        install -d -o "$HERMIT_USER" -g "$HERMIT_GROUP" -m 0750 "${STATE_DIR}/librespot"

        if write_if_changed /etc/systemd/system/hermit-librespot.service \
"[Unit]
Description=HERMIT: librespot Spotify Connect sidecar
After=network-online.target sound.target avahi-daemon.service
Wants=network-online.target

[Service]
Type=simple
User=${HERMIT_USER}
Group=${HERMIT_GROUP}
SupplementaryGroups=audio

# --backend alsa --device default puts Spotify audio through the same dmix
# chain as everything else, which is what keeps the hardware AEC valid while
# music plays.  --bitrate 160 is deliberate: this is a 3 W mono speaker fed by
# a 16 kHz-class pipeline, 320 kbit buys nothing audible and costs bandwidth
# and CPU on a 1 GB Pi.
# NOTE: librespot's CLI has changed across releases.  If this unit fails with
# 'unexpected argument', run 'librespot --help' and adjust; the flags below
# target librespot 0.4-0.6.
ExecStart=${librespot_bin} \\
    --name Hermit \\
    --backend alsa \\
    --device default \\
    --bitrate 160 \\
    --initial-volume 60 \\
    --volume-ctrl linear \\
    --cache ${STATE_DIR}/librespot \\
    --disable-audio-cache

Restart=always
RestartSec=5

NoNewPrivileges=yes
PrivateTmp=yes
ProtectHome=yes
ProtectSystem=strict
ReadWritePaths=${STATE_DIR}
PrivateDevices=no
DevicePolicy=closed
DeviceAllow=char-alsa rw
MemoryMax=128M

[Install]
WantedBy=multi-user.target" 0644
        then
            changed "installed hermit-librespot.service"
        else
            log "hermit-librespot.service already up to date"
        fi
    else
        warn "librespot not installed; skipping hermit-librespot.service"
    fi

    # --- shairport-sync: OPTIONAL, off by default ---------------------------
    # AirPlay 2 receiver.  It is the ONLY way to get Apple Music onto this box:
    # there is no Apple Music client for Linux and no API that lets an agent
    # start Apple Music playback, so the agent CANNOT drive Apple Music itself.
    # With shairport-sync a human AirPlays to the device from a phone or Mac and
    # HERMIT is just a speaker for that stream.  Off by default because it is
    # another always-on network listener for a feature the agent cannot use.
    if ! dpkg-query -W -f='${Status}' shairport-sync 2>/dev/null | grep -q "ok installed"; then
        log "shairport-sync (AirPlay 2) not installed — optional, off by default"
        log "  to enable AirPlay:  sudo apt-get install -y shairport-sync"
        log "  then set its ALSA output device to 'default' in /etc/shairport-sync.conf"
        log "  NOTE: AirPlay is the only Apple Music path on Linux, and it is human-"
        log "        driven only. The agent cannot start or control Apple Music."
    else
        log "shairport-sync is installed; make sure its output device is 'default'"
    fi
}


# ===========================================================================
# 13. Install the HERMIT systemd units from the repo.
# ===========================================================================

install_units() {
    step "systemd units"

    local unit
    for unit in hermit.service hermit-consolidate.service hermit-consolidate.timer; do
        [[ -f "${DEPLOY_DIR}/${unit}" ]] || { warn "missing ${DEPLOY_DIR}/${unit}, skipping"; continue; }
        if ! cmp -s "${DEPLOY_DIR}/${unit}" "/etc/systemd/system/${unit}"; then
            install -m 0644 -o root -g root "${DEPLOY_DIR}/${unit}" "/etc/systemd/system/${unit}"
            changed "installed ${unit}"
        else
            log "${unit} already up to date"
        fi
    done

    systemctl daemon-reload

    # The nightly consolidation timer is safe to enable and start now: it will
    # not fire until 04:00, by which point the binary is expected to be there.
    systemctl enable --now hermit-consolidate.timer >/dev/null 2>&1 \
        && changed "enabled hermit-consolidate.timer (daily 04:00)" \
        || warn "could not enable hermit-consolidate.timer"

    # Sidecars: enable so they come up at boot.
    systemctl enable hermit-mpv.service >/dev/null 2>&1 || true
    [[ -f /etc/systemd/system/hermit-librespot.service ]] \
        && systemctl enable hermit-librespot.service >/dev/null 2>&1 || true

    # hermit.service is ENABLED but NOT started: the binary may not be here
    # yet, and a Type=notify unit with no executable just burns restart budget.
    systemctl enable hermit.service >/dev/null 2>&1 \
        && log "hermit.service enabled (not started — see the summary)" \
        || warn "could not enable hermit.service"

    if [[ -x "${OPT_DIR}/bin/hermit" ]]; then
        log "binary is present at ${OPT_DIR}/bin/hermit"
    else
        todo "Copy the cross-compiled binary to ${OPT_DIR}/bin/hermit and the data files
       to ${OPT_DIR}/config, then:  sudo systemctl start hermit
       (See hermit/deploy/README.md for the exact rsync/scp lines.)"
    fi

    if [[ ! -f "${OPT_DIR}/config/hermit.toml" ]]; then
        todo "Copy the config tree to ${OPT_DIR}/config — it needs hermit.toml, prompts/,
       skills/, identity.md, core.md and stations.toml."
    fi
}


# ===========================================================================
# 14. Summary.
# ===========================================================================

print_summary() {
    printf '\n\033[1;36m============================================================\033[0m\n'
    printf '\033[1;36m  HERMIT provisioning complete\033[0m\n'
    printf '\033[1;36m============================================================\033[0m\n\n'

    if [[ ${#CHANGES[@]} -eq 0 ]]; then
        printf '  Nothing changed — this machine was already provisioned.\n\n'
    else
        printf '  \033[1mChanged on this run (%d):\033[0m\n' "${#CHANGES[@]}"
        local c
        for c in "${CHANGES[@]}"; do printf '    * %s\n' "$c"; done
        printf '\n'
    fi

    printf '  \033[1mLayout:\033[0m\n'
    printf '    binary       %s/bin/hermit\n'   "$OPT_DIR"
    printf '    config       %s/config/\n'      "$OPT_DIR"
    printf '    state + db   %s/\n'             "$STATE_DIR"
    printf '    secrets      %s\n'              "$ENV_FILE"
    printf '    runtime      %s/ (mpv.sock)\n'  "$RUN_DIR"
    printf '    alsa         /etc/asound.conf\n\n'

    printf '  \033[1mUnits:\033[0m\n'
    printf '    hermit.service               main daemon (Type=notify, watchdog 30s)\n'
    printf '    hermit-consolidate.timer     nightly memory consolidation, 04:00\n'
    printf '    hermit-mpv.service           internet radio sidecar\n'
    printf '    hermit-librespot.service     Spotify Connect sidecar (if installed)\n'
    printf '    hermit-cpu-governor.service  pins the CPU to performance at boot\n\n'

    if [[ ${#MANUAL[@]} -gt 0 ]]; then
        printf '  \033[1;33mSTILL TO DO BY HAND (%d):\033[0m\n\n' "${#MANUAL[@]}"
        local m i=1
        for m in "${MANUAL[@]}"; do
            printf '  \033[1;33m%d.\033[0m %s\n\n' "$i" "$m"
            i=$((i + 1))
        done
    else
        printf '  \033[0;32mNo outstanding manual steps.\033[0m\n\n'
    fi

    printf '  \033[1mUseful:\033[0m\n'
    printf '    journalctl -u hermit -f          follow the daemon\n'
    printf '    systemctl status hermit          state, watchdog, memory\n'
    printf '    aplay -l ; arecord -l            confirm the Flex is the only card\n'
    printf '    alsamixer -c <card>              levels (ceiling is in asound.conf)\n'
    printf '    vcgencmd measure_temp            thermals\n'
    printf '    vcgencmd get_throttled           0x0 means never throttled\n\n'

    if [[ $REBOOT_REQUIRED -eq 1 ]]; then
        printf '  \033[1;33mA REBOOT IS REQUIRED\033[0m — boot config and/or module blacklist changed.\n'
        printf '    sudo reboot\n'
        printf '  After the reboot, re-run this script so the ALSA mixer step can\n'
        printf '  run against the card in its final enumeration order.\n\n'
    fi
}


# ===========================================================================
main() {
    preflight
    create_user
    create_dirs
    install_packages
    configure_swap
    configure_boot
    trim_services
    configure_governor
    configure_time
    configure_journald
    configure_alsa
    create_env_template
    install_sidecars
    install_units
    print_summary
}

main "$@"
