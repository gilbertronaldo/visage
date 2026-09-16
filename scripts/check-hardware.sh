#!/bin/sh
#
# Visage hardware pre-check — answers "will this work on my laptop?" before you
# build or install anything.
#
#   ./scripts/check-hardware.sh
#
# Needs only a POSIX shell and coreutils. It reads sysfs, never opens a camera,
# never needs root, and changes nothing. Run it straight after cloning — the
# point is to fail fast, before you install a Rust toolchain and download 182 MB
# of ONNX models for a camera Visage cannot drive.
#
# Exit status: 0 a usable IR camera was found, 1 none was, 2 nothing to inspect.

set -eu

# Overridable so the negative paths (quirk hit, IPU6, no devices) can be exercised
# against a synthetic tree — otherwise they could only ever be tested by owning
# one of every camera.
SYSFS=${VISAGE_CHECK_SYSFS:-/sys/class/video4linux}

# Locate the quirk database relative to this script, so the check can never
# disagree with what the binary would embed.
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
QUIRK_DIR="$SCRIPT_DIR/../contrib/hw"

if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
    B=$(printf '\033[1m'); R=$(printf '\033[0m')
    GRN=$(printf '\033[32m'); YEL=$(printf '\033[33m'); RED=$(printf '\033[31m')
else
    B=''; R=''; GRN=''; YEL=''; RED=''
fi

# ── Quirk lookup ──────────────────────────────────────────────────────────────
# Quirk files declare `vendor_id = 0x04F2` / `product_id = 0xB6D9`; sysfs reports
# lowercase hex with no prefix. Normalise both to uppercase bare hex.

quirk_name_for() {   # $1=vid $2=pid  → prints the quirk's name, or nothing
    _want_vid=$(printf '%s' "$1" | tr 'a-f' 'A-F')
    _want_pid=$(printf '%s' "$2" | tr 'a-f' 'A-F')
    [ -d "$QUIRK_DIR" ] || return 0
    for _q in "$QUIRK_DIR"/*.toml; do
        [ -f "$_q" ] || continue
        _vid=$(sed -n 's/^[[:space:]]*vendor_id[[:space:]]*=[[:space:]]*0[xX]\([0-9A-Fa-f]*\).*/\1/p'  "$_q" | head -1 | tr 'a-f' 'A-F')
        _pid=$(sed -n 's/^[[:space:]]*product_id[[:space:]]*=[[:space:]]*0[xX]\([0-9A-Fa-f]*\).*/\1/p' "$_q" | head -1 | tr 'a-f' 'A-F')
        # Compare numerically-equal hex without leading-zero surprises.
        [ "$((0x0$_vid))" = "$((0x0$_want_vid))" ] || continue
        [ "$((0x0$_pid))" = "$((0x0$_want_pid))" ] || continue
        sed -n 's/^[[:space:]]*name[[:space:]]*=[[:space:]]*"\(.*\)".*/\1/p' "$_q" | head -1
        return 0
    done
}

# ── Testable seam ─────────────────────────────────────────────────────────────
# `--quirk-lookup VID PID` prints the matching quirk's name and exits. This is
# what lets a test assert that this script's TOML parsing agrees with the quirk
# database the binary embeds, rather than the two drifting apart silently — a
# class of defect this repository has already shipped twice.

case "${1:-}" in
    --quirk-lookup)
        [ $# -eq 3 ] || { echo "usage: $0 --quirk-lookup VID PID" >&2; exit 64; }
        quirk_name_for "$2" "$3"
        exit 0
        ;;
    --help|-h)
        sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'
        exit 0
        ;;
    "") : ;;
    *)  echo "unknown argument: $1 (try --help)" >&2; exit 64 ;;
esac

# ── IR detection ──────────────────────────────────────────────────────────────
# A single USB camera can expose BOTH an RGB and an IR function under one
# VID:PID — measured on Shinetech 3277:0055, where /dev/video0 is
# "USB2.0 FHD UVC WebCam" and /dev/video2 is "USB2.0 IR UVC WebCam". The USB
# interface string names the function and is not truncated; the sysfs `name` is
# capped at 32 bytes and can cut the distinguishing word off. Prefer `interface`.

looks_like_ir() {   # $1=text → 0 if it names an IR/infrared function
    # Replace non-alphanumerics with spaces so " IR " matches "IR-Camera" and
    # "USB2.0_IR" but not "FIRE", "WIRELESS" or "THIRD".
    _norm=$(printf ' %s ' "$1" | tr -c '[:alnum:]' ' ' | tr 'a-z' 'A-Z')
    case "$_norm" in
        *" IR "*|*" INFRARED "*|*" IRCAM "*) return 0 ;;
    esac
    return 1
}

# ── Enumerate ─────────────────────────────────────────────────────────────────

if [ ! -d "$SYSFS" ]; then
    printf '%sNo V4L2 subsystem on this machine%s (%s is absent).\n' "$RED" "$R" "$SYSFS"
    printf 'Visage needs a video4linux camera. Nothing to check.\n'
    exit 2
fi

printf '%sVisage hardware check%s\n\n' "$B" "$R"

# Without the quirk database every camera would silently report "no quirk on
# file" — which is a real, common, benign verdict, so the degradation would be
# indistinguishable from a correct answer. Say so instead.
if [ ! -d "$QUIRK_DIR" ]; then
    printf '  %sNote:%s quirk database not found at %s\n' "$YEL" "$R" "$QUIRK_DIR"
    printf '  Run this from a checkout of the repository. Without it, cameras that\n'
    printf '  DO have a verified emitter quirk will be reported as merely likely.\n\n'
fi

found_any=0; found_usable=0; found_ipu6=0; found_rgb_only=0

for dev in "$SYSFS"/video*; do
    [ -e "$dev" ] || continue
    node=$(basename "$dev")

    # V4L2 exposes a metadata node alongside each capture node; only index 0
    # captures. Skip the rest so one camera is reported once.
    idx=$(cat "$dev/index" 2>/dev/null || echo 0)
    [ "$idx" = "0" ] || continue

    found_any=$((found_any + 1))

    name=$(cat "$dev/name" 2>/dev/null || echo '(unnamed)')
    iface=$(cat "$dev/device/interface" 2>/dev/null || true)
    driver=$(readlink "$dev/device/driver" 2>/dev/null | sed 's#.*/##' || true)
    [ -n "$driver" ] || driver='(none)'

    vid=''; pid=''
    if ifpath=$(readlink -f "$dev/device" 2>/dev/null); then
        usbdir=$(dirname "$ifpath")
        vid=$(cat "$usbdir/idVendor"  2>/dev/null || true)
        pid=$(cat "$usbdir/idProduct" 2>/dev/null || true)
    fi

    # The interface string names the function; the node name may be truncated.
    label=${iface:-$name}

    printf '  %s/dev/%s%s  %s\n' "$B" "$node" "$R" "$label"
    if [ -n "$vid" ] && [ -n "$pid" ]; then
        printf '      usb %s:%s   driver %s\n' "$vid" "$pid" "$driver"
    else
        printf '      non-USB      driver %s\n' "$driver"
    fi

    case "$driver" in
        *ipu6*|*intel_ipu*)
            found_ipu6=$((found_ipu6 + 1))
            printf '      %sNot supported%s — Intel IPU6 needs the proprietary camera HAL,\n' "$RED" "$R"
            printf '      not V4L2. Visage cannot drive this camera.\n'
            ;;
        uvcvideo)
            if looks_like_ir "$label"; then
                quirk=''
                [ -n "$vid" ] && [ -n "$pid" ] && quirk=$(quirk_name_for "$vid" "$pid")
                if [ -n "$quirk" ]; then
                    found_usable=$((found_usable + 1))
                    printf '      %sSupported%s — IR camera with a verified emitter quirk:\n' "$GRN" "$R"
                    printf '      %s\n' "$quirk"
                else
                    found_usable=$((found_usable + 1))
                    printf '      %sLikely supported%s — IR camera, no emitter quirk on file.\n' "$YEL" "$R"
                    printf '      That is common and often fine: many modules strobe the\n'
                    printf '      emitter by firmware default and need no quirk at all.\n'
                    printf '      If enrollment gives dark frames, a quirk is what is missing —\n'
                    printf '      see contrib/hw/README.md, and please send a hardware report.\n'
                fi
            else
                found_rgb_only=$((found_rgb_only + 1))
                printf '      %sRGB only%s — usable for testing, but not for authentication.\n' "$YEL" "$R"
                printf '      A colour webcam can be fooled by a photograph.\n'
            fi
            ;;
        *)
            printf '      %sUnknown%s — driver "%s" is neither uvcvideo nor IPU6.\n' "$YEL" "$R" "$driver"
            printf '      Visage speaks V4L2/UVC; this may or may not work.\n'
            ;;
    esac
    printf '\n'
done

# ── Verdict ───────────────────────────────────────────────────────────────────

if [ "$found_any" -eq 0 ]; then
    printf '%sNo capture devices found.%s\n' "$RED" "$R"
    printf 'If your camera is disabled in firmware or by a privacy switch, enable it and re-run.\n'
    exit 2
fi

if [ "$found_usable" -gt 0 ]; then
    printf '%sGood — %d IR camera(s) Visage can use.%s\n\n' "$GRN" "$found_usable" "$R"
    printf 'Next: %s./scripts/quickstart.sh%s, or install a package and run %ssudo visage onboard%s.\n' "$B" "$R" "$B" "$R"
    exit 0
fi

printf '%sNo IR camera Visage can use.%s\n\n' "$RED" "$R"
if [ "$found_ipu6" -gt 0 ]; then
    printf 'This machine has an Intel IPU6 camera. Support for IPU6 is a separate\n'
    printf 'milestone and is not available today.\n'
fi
if [ "$found_rgb_only" -gt 0 ]; then
    printf 'Only colour (RGB) cameras were found. Face authentication needs an\n'
    printf 'infrared camera — the kind marketed as "Windows Hello" — because a\n'
    printf 'colour camera cannot tell a face from a photograph of one.\n'
fi
printf '\nFull compatibility detail: docs/hardware-compatibility.md\n'
exit 1
