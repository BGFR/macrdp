#!/bin/bash
# udp-block.sh — block ONLY UDP on the macrdp port (default 3390), both
# directions, via a pf anchor. TCP is untouched, so the RDP session itself
# stays up while the UDP multitransport tunnel goes dark.
#
# Purpose: live-verify the tunnel-death detection + offer cooldown (#133).
# With a client connected and a tunnel BOUND (e.g. ENABLE_LOSSY_AUDIO=1),
# `on` starves the tunnel of inbound datagrams; after
# MACRDP_UDP_TUNNEL_DEAD_SECS (default 30 s) of silence the listener must:
#   - log "UDP tunnel DEAD (bound peer inbound-silent past the threshold)"
#   - fall lossy audio back to TCP immediately (audio keeps playing)
#   - suppress multitransport offers for MACRDP_UDP_MT_COOLDOWN_SECS (600 s)
# so a client-side dead-tunnel reset (if mstsc fires one at ~60 s) reconnects
# as a stable plain-TCP session — at most ONE reset, no cycle.
#
#   sudo scripts/udp-block.sh on
#   sudo scripts/udp-block.sh off
#   scripts/udp-block.sh status

set -euo pipefail

PORT="${PORT:-3390}"
ANCHOR="macrdp-udpblock"
cmd="${1:-status}"

require_root() {
    [ "$(id -u)" = 0 ] || { echo "need sudo: sudo $0 $cmd" >&2; exit 1; }
}

case "$cmd" in
    on)
        require_root
        # Load the default ruleset plus our anchor declaration, then populate
        # the anchor (same pattern as netshape.sh — never clobbers other rules).
        {
            cat /etc/pf.conf 2>/dev/null || true
            echo "anchor \"$ANCHOR\""
        } | pfctl -q -f -
        # Both directions: client→server has DESTINATION port $PORT, the
        # server→client replies have SOURCE port $PORT (see netshape.sh).
        echo "block drop quick proto udp from any to any port $PORT
block drop quick proto udp from any port $PORT to any" \
            | pfctl -q -a "$ANCHOR" -f -
        pfctl -q -E 2>/dev/null || pfctl -q -e 2>/dev/null || true
        echo "UDP blocked on port $PORT (TCP untouched). Clear with: sudo $0 off"
        ;;

    off)
        require_root
        pfctl -q -a "$ANCHOR" -F all 2>/dev/null || true
        pfctl -q -f /etc/pf.conf 2>/dev/null || true
        # Leave pf as found on a stock Mac (idle). If you rely on pf, re-enable.
        pfctl -q -d 2>/dev/null || true
        echo "done."
        ;;

    status)
        echo "=== pf anchor $ANCHOR ==="
        pfctl -q -a "$ANCHOR" -s rules 2>/dev/null || echo "(none / need sudo)"
        ;;

    *)
        echo "usage: sudo $0 on|off|status" >&2
        exit 2
        ;;
esac
