#!/bin/bash
# soak_abuse.sh — DEFENSIVE resilience self-test for macrdp (this project's own
# RDP server). Fires a bounded burst of hostile-shaped connections at a macrdp
# instance to confirm it stays up, leak-free, and that the auth-guard rate-limit/
# lockout and the protocol decoders hold. Red-team-your-own-service tooling, in
# the same spirit as a fuzz harness or a load test.
#
# ONLY run this against a macrdp server YOU operate. It is deliberately abusive
# (connection floods, malformed frames, half-open holds) — do not point it at a
# host you don't own.
#
# Bounded + safe by design: normal connect() only (no raw-packet/SYN flood),
# capped connection counts, no disk-fill, no unbounded forks. NOTE the auth-guard
# is loopback-EXEMPT, so to actually exercise the rate-limit/lockout you must run
# from a NON-loopback host against the server's LAN/network address.
#
# Each phase targets a documented protection.
# Usage: ./soak_abuse.sh [host] [port]     (default 127.0.0.1:3390)
set -u
HOST="${1:-127.0.0.1}"
PORT="${2:-3390}"
say() { printf '\n=== %s ===\n' "$*"; }

# Open a TCP connection via bash /dev/tcp, optionally send bytes, optionally hold.
connect() { # $1=payload-file-or-empty  $2=hold-seconds
  local payload="$1" hold="${2:-0}"
  exec 3<>"/dev/tcp/$HOST/$PORT" 2>/dev/null || return 1
  [ -n "$payload" ] && [ -f "$payload" ] && cat "$payload" >&3 2>/dev/null
  [ "$hold" -gt 0 ] && sleep "$hold"
  exec 3>&- 3<&- 2>/dev/null
}

say "PHASE 1 — connection-rate flood (60 rapid connect+close from one IP)"
echo "  expect: auth-guard rate-limiter rejects the excess pre-handshake (>10/60s)."
for i in $(seq 1 60); do connect "" 0 & done; wait
echo "  60 connect+close attempts fired."

say "PHASE 2 — malformed payloads (40 connect + random junk, not a valid X.224 CR)"
echo "  expect: protocol decoders reject cleanly (no panic); errors count toward lockout."
JUNK=$(mktemp); head -c 512 /dev/urandom > "$JUNK"
for i in $(seq 1 40); do connect "$JUNK" 0 & done; wait
# A few oversized junk payloads too (decoder length-bound check).
BIGJUNK=$(mktemp); head -c 65535 /dev/urandom > "$BIGJUNK"
for i in $(seq 1 10); do connect "$BIGJUNK" 0 & done; wait
rm -f "$JUNK" "$BIGJUNK"
echo "  50 malformed-payload attempts fired."

say "PHASE 3 — half-open / slow-hold (40 connections held open ~12s, sending nothing)"
echo "  expect: no fd/memory pileup; connections time out or sit bounded."
for i in $(seq 1 40); do connect "" 12 & done
echo "  40 half-open connections holding (12s)..."; wait
echo "  half-open batch released."

say "PHASE 4 — concurrent burst (80 simultaneous connect, brief hold)"
echo "  expect: single-session model + backlog absorb it; fd bounded, no crash."
for i in $(seq 1 80); do connect "" 3 & done; wait
echo "  80 concurrent connections cycled."

say "PHASE 5 — sustained rate loop (5 rounds of 20 connect+close, ~1s apart)"
echo "  expect: lockout escalates + auto-expires; server stays up throughout."
for round in $(seq 1 5); do
  for i in $(seq 1 20); do connect "" 0 & done; wait
  echo "  round $round: 20 fired"; sleep 1
done

say "DONE — abuse complete"
echo "  Total: ~370 connection attempts across 5 phases from this IP."
echo "  Now snapshot the server: pid unchanged? panics=0? RSS/FD bounded? guard engaged?"
