#!/bin/bash
# Pick an attached USB device to use as the smart-card IFD-handler load trigger.
#
# Why this exists: macOS loads a third-party PC/SC IFD driver ONLY on a USB
# *hotplug* whose VID/PID match the bundle's Info.plist, so smart-card
# redirection needs some USB device permanently attached as a trigger (any stick
# works). Finding a device's VID/PID by hand (digging through `system_profiler`
# or `ioreg` and converting hex) is the fiddly part — this lists the attached
# devices, lets you pick one, and prints its VID/PID.
#
# Output contract (so it's scriptable):
#   * stdout: the chosen "<vid> <pid>" (e.g. "0x2174 0x2100"), nothing else.
#   * stderr: the menu, prompts, and a ready-to-paste install command.
#   * exit 0 = a device was chosen; non-zero = skipped / none found / aborted.
# So install-ifd-handler.sh can do:  sel="$(select-usb-trigger.sh)" && bind $sel
set -euo pipefail

# Enumerate USB devices via system_profiler JSON (VID/PID already come as 0x..
# strings there; ioreg reports them in decimal). Flatten the hub tree, keep only
# leaves that expose both a hex vendor_id and a product_id. Emits TSV:
#   <vid>\t<pid>\t<label>
list_devices() {
    # NOTE: `python3 -c`, NOT `python3 - <<HEREDOC` — a heredoc would BECOME
    # python's stdin, so json.load(sys.stdin) would read the script instead of
    # the piped JSON. With -c the pipe stays stdin. The python body is single-
    # quote-safe (only double quotes / f-strings inside). Each match is printed
    # newline-terminated so bash `while read` doesn't drop the last line.
    system_profiler -json SPUSBDataType 2>/dev/null | /usr/bin/python3 -c '
import json, sys, re
def walk(items, out):
    for it in items or []:
        pid = it.get("product_id", "")
        m = re.search(r"0x[0-9a-fA-F]+", it.get("vendor_id", ""))
        if pid.startswith("0x") and m:
            name = it.get("_name", "?")
            man = it.get("manufacturer", "")
            label = name if (not man or man in name) else f"{name} ({man})"
            vid = "0x%04x" % int(m.group(0), 16)   # 4-digit lc hex, as Info.plist wants
            pidn = "0x%04x" % int(pid, 16)
            out.append(f"{vid}\t{pidn}\t{label}")
        walk(it.get("_items"), out)
out = []
try:
    d = json.load(sys.stdin)
except Exception:
    d = {}
walk(d.get("SPUSBDataType"), out)
for line in out:
    print(line)
' 2>/dev/null || true
}

# bash 3.2: no mapfile/assoc arrays. Build parallel indexed arrays in a loop.
VIDS=(); PIDS=(); LABELS=(); n=0
while IFS=$'\t' read -r vid pid label; do
    [ -n "$vid" ] || continue
    VIDS[$n]="$vid"; PIDS[$n]="$pid"; LABELS[$n]="$label"
    n=$((n + 1))
done < <(list_devices)

if [ "$n" -eq 0 ]; then
    echo "No USB devices found. Plug in the dongle you want to use as the trigger" >&2
    echo "(any USB stick works), then re-run. Falling back to the bundle default." >&2
    exit 1
fi

# A TTY to read the choice from, even when stdout is captured by a caller.
# (MACRDP_USB_TTY overrides it — used by the test harness; defaults to /dev/tty.)
TTY="${MACRDP_USB_TTY:-/dev/tty}"
[ -e "$TTY" ] || { echo "no controlling terminal; cannot prompt for a USB pick" >&2; exit 1; }

{
    echo "Attached USB devices — pick one as the smart-card load trigger:"
    i=0
    while [ "$i" -lt "$n" ]; do
        printf "  %2d) %-40s  VID=%s PID=%s\n" "$((i + 1))" "${LABELS[$i]}" "${VIDS[$i]}" "${PIDS[$i]}"
        i=$((i + 1))
    done
    echo "   0) skip (keep the bundle's default trigger)"
} >&2

printf "Selection [0-%d]: " "$n" >&2
read -r choice < "$TTY" || { echo >&2; echo "aborted" >&2; exit 1; }

case "$choice" in
    ''|0) echo "Keeping the default trigger." >&2; exit 1 ;;
    *[!0-9]*) echo "Not a number: $choice" >&2; exit 1 ;;
esac
if [ "$choice" -lt 1 ] || [ "$choice" -gt "$n" ]; then
    echo "Out of range: $choice" >&2; exit 1
fi

idx=$((choice - 1))
vid="${VIDS[$idx]}"; pid="${PIDS[$idx]}"
{
    echo
    echo "Selected: ${LABELS[$idx]}  (VID=$vid PID=$pid)"
    echo "Bind it during install with:"
    echo "    IFD_VID=$vid IFD_PID=$pid packaging/install-ifd-handler.sh"
} >&2
echo "$vid $pid"
