#!/bin/bash
# soak_abuse2.sh — DEFENSIVE resilience self-test for macrdp (this project's own
# RDP server): handshake-phase abuse, the companion to soak_abuse.sh. Unlike that
# script's pre-TLS garbage, these send a VALID X.224 Connection Request so they
# reach the handshake, then either stall it or drop.
#   #1 handshake-phase slowloris — send CR, then stall the TLS handshake / byte-drip.
#   #2 valid-CR-then-drop flood  — send CR, then close (port-scan / preempt-probe shape).
#
# ONLY run this against a macrdp server YOU operate. It is deliberately abusive —
# do not point it at a host you don't own. Bounded + safe by design. Run from a
# NON-loopback host to exercise the (loopback-exempt) auth-guard.
#
# Usage: ./soak_abuse2.sh [host] [port]    (default 127.0.0.1:3390)
set -u
HOST="${1:-127.0.0.1}"
PORT="${2:-3390}"
say() { printf '\n=== %s ===\n' "$*"; }

# A minimal valid X.224 Connection Request requesting SSL|HYBRID:
#   TPKT  03 00 00 13                (ver 3, len 19)
#   X.224 0e e0 0000 0000 00         (LI=14, CR, dst/src ref, class)
#   rdpNegReq 01 00 0800 03000000    (TYPE_RDP_NEG_REQ, PROTOCOL_SSL|HYBRID)
CR=$'\x03\x00\x00\x13\x0e\xe0\x00\x00\x00\x00\x00\x01\x00\x08\x00\x03\x00\x00\x00'

send_cr() { printf '%s' "$CR"; }

# connect, send the CR, optional hold (stall the post-CR TLS handshake), close.
cr_then_hold() { # $1=hold-seconds
  exec 3<>"/dev/tcp/$HOST/$PORT" 2>/dev/null || return 1
  send_cr >&3 2>/dev/null
  [ "${1:-0}" -gt 0 ] && sleep "$1"
  exec 3>&- 3<&- 2>/dev/null
}

# connect, drip the CR one byte at a time over ~time, then stall, then close.
cr_bytedrip() { # $1=total-seconds
  exec 3<>"/dev/tcp/$HOST/$PORT" 2>/dev/null || return 1
  local n=${#CR} i delay
  delay=$(awk -v t="${1:-15}" -v n="$n" 'BEGIN{printf "%.2f", t/n}')
  for ((i=0;i<n;i++)); do printf '%s' "${CR:i:1}" >&3 2>/dev/null; sleep "$delay"; done
  sleep 3
  exec 3>&- 3<&- 2>/dev/null
}

say "PHASE A — handshake-phase slowloris (valid CR, then stall the TLS handshake)"
echo "  batch of 8 (under the 10/60s rate-limit so they reach the handshake), held 25s."
echo "  expect: server times out the stalled TLS phase; no fd/mem pileup, no crash."
for i in $(seq 1 8); do cr_then_hold 25 & done
echo "  8 stalled handshakes holding (25s)..."
# plus 3 byte-drip connections in parallel (very slow send)
for i in 1 2 3; do cr_bytedrip 18 & done
wait
echo "  slowloris + byte-drip batch released."

say "PHASE B — valid-CR-then-drop flood (60 rapid CR + close)"
echo "  expect: rate-limiter rejects the excess even though the framing is valid;"
echo "          with a live session, exercises the div-23 preemption probe (CR then gone)."
for i in $(seq 1 60); do cr_then_hold 0 & done; wait
echo "  60 valid-CR-then-drop attempts fired."

say "PHASE C — mixed sustained (5 rounds: 6 slow-hold + 15 CR-drop)"
echo "  expect: lockout escalates + auto-expires; server stays a single stable process."
for round in $(seq 1 5); do
  for i in $(seq 1 6); do cr_then_hold 8 & done
  for i in $(seq 1 15); do cr_then_hold 0 & done
  wait
  echo "  round $round done"; sleep 1
done

say "DONE — handshake-phase abuse complete"
echo "  Snapshot the server: pid unchanged? panics=0? RSS/FD returned to baseline? guard engaged?"
