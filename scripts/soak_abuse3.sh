#!/bin/bash
# soak_abuse3.sh — DEFENSIVE resilience self-test for macrdp (this project's own
# RDP server): targeted PROTOCOL-DECODER fuzzing + auth-guard behavioral edges.
# The third companion to soak_abuse.sh / soak_abuse2.sh. Where those send random
# junk (fails at the first byte) or ONE fixed valid CR (slowloris/drop), this one
# sends structurally-plausible-but-MALFORMED framing that reaches deeper into the
# real parser — the X.224 negotiation request and the TPKT framing reader — plus
# it walks the guard's lockout escalation and its per-IP isolation.
#
# ONLY run this against a macrdp server YOU operate. It is deliberately abusive —
# do not point it at a host you don't own. Bounded + safe by design (normal
# connect() only, capped counts, short holds, no raw-packet/SYN flood).
#
# RUN IT TWICE — the two halves want opposite conditions, because the auth-guard
# rejects PRE-HANDSHAKE, so a guard-rejected connection never reaches the decoder:
#   • DECODER fuzzing (Phase A + B) wants LOOPBACK — the guard is loopback-EXEMPT,
#     so every malformed variant actually lands on the parser. Run: ./soak_abuse3.sh
#   • GUARD behavior (Phase C + D) wants a NON-LOOPBACK run against the server's
#     LAN / overlay address, so the guard engages. Run:
#       ./soak_abuse3.sh <lan-ip> 3390 <alt-ip>
#     (arg 3 = a SECOND address of the SAME server — e.g. its ZeroTier IP — used
#      for the per-IP-isolation phase; omit it to skip Phase D.)
#
# Usage: ./soak_abuse3.sh [host] [port] [alt_host]     (default 127.0.0.1 3390)
# Env:   STEPS=N   lockout-escalation steps to demonstrate in Phase C (default 3:
#                  30->60->120s; raise toward the ~900s cap, each step costs its
#                  own cooldown wait).
set -u
HOST="${1:-127.0.0.1}"
PORT="${2:-3390}"
ALT="${3:-}"
STEPS="${STEPS:-3}"
say() { printf '\n=== %s ===\n' "$*"; }

case "$HOST" in 127.*|::1|localhost) LOOPBACK=1;; *) LOOPBACK=0;; esac

# Emit exact bytes (NUL-safe): the payload holds literal \xNN escapes + ASCII, and
# printf interprets them straight to the socket (a bash $'..' VARIABLE can drop
# embedded NULs on some platforms; the printf-format path does not).
fire() { # $1=payload-with-\xNN-escapes  $2=hold-seconds
  exec 3<>"/dev/tcp/$HOST/$PORT" 2>/dev/null || return 1
  printf "$1" >&3 2>/dev/null
  [ "${2:-0}" -gt 0 ] && sleep "$2"
  exec 3>&- 3<&- 2>/dev/null
}
fire_to() { # $1=host  $2=payload  $3=hold
  exec 3<>"/dev/tcp/$1/$PORT" 2>/dev/null || return 1
  printf "$2" >&3 2>/dev/null
  [ "${3:-0}" -gt 0 ] && sleep "$3"
  exec 3>&- 3<&- 2>/dev/null
}

# --- A well-formed X.224 Connection Request (SSL|HYBRID), as the mutation base ---
#   TPKT  03 00 00 13            (ver 3, len 19)
#   X.224 0e e0 0000 0000 00     (LI=14, CR, dst/src ref, class 0)
#   Nego  01 00 0800 03000000    (TYPE_RDP_NEG_REQ, flags 0, len 8, SSL|HYBRID)
CR='\x03\x00\x00\x13\x0e\xe0\x00\x00\x00\x00\x00\x01\x00\x08\x00\x03\x00\x00\x00'

# --- Phase A variants: valid framing, malformed rdpNegReq ---------------------
V_BADTYPE='\x03\x00\x00\x13\x0e\xe0\x00\x00\x00\x00\x00\xff\x00\x08\x00\x03\x00\x00\x00'  # nego TYPE 0xff (not RDP_NEG_REQ)
V_P00='\x03\x00\x00\x13\x0e\xe0\x00\x00\x00\x00\x00\x01\x00\x08\x00\x00\x00\x00\x00'       # requestedProtocols = RDP (0)
V_P01='\x03\x00\x00\x13\x0e\xe0\x00\x00\x00\x00\x00\x01\x00\x08\x00\x01\x00\x00\x00'       # = SSL
V_P02='\x03\x00\x00\x13\x0e\xe0\x00\x00\x00\x00\x00\x01\x00\x08\x00\x02\x00\x00\x00'       # = HYBRID
V_P04='\x03\x00\x00\x13\x0e\xe0\x00\x00\x00\x00\x00\x01\x00\x08\x00\x04\x00\x00\x00'       # = RDSTLS
V_P08='\x03\x00\x00\x13\x0e\xe0\x00\x00\x00\x00\x00\x01\x00\x08\x00\x08\x00\x00\x00'       # = HYBRID_EX
V_P0B='\x03\x00\x00\x13\x0e\xe0\x00\x00\x00\x00\x00\x01\x00\x08\x00\x0b\x00\x00\x00'       # = SSL|HYBRID|HYBRID_EX
V_PFF='\x03\x00\x00\x13\x0e\xe0\x00\x00\x00\x00\x00\x01\x00\x08\x00\xff\xff\xff\xff'       # = all bits (invalid)
V_NLEN0='\x03\x00\x00\x13\x0e\xe0\x00\x00\x00\x00\x00\x01\x00\x00\x00\x03\x00\x00\x00'     # nego length field = 0
V_NLENFF='\x03\x00\x00\x13\x0e\xe0\x00\x00\x00\x00\x00\x01\x00\xff\xff\x03\x00\x00\x00'    # nego length field = 0xffff (frame short)
V_LIFF='\x03\x00\x00\x13\xff\xe0\x00\x00\x00\x00\x00\x01\x00\x08\x00\x03\x00\x00\x00'      # X.224 LI = 0xff (overlong)
V_LI00='\x03\x00\x00\x13\x00\xe0\x00\x00\x00\x00\x00\x01\x00\x08\x00\x03\x00\x00\x00'      # X.224 LI = 0
V_COOKIE='\x03\x00\x00\x2a\x25\xe0\x00\x00\x00\x00\x00Cookie: mstshash=soak\r\n\x01\x00\x08\x00\x03\x00\x00\x00'  # routing token + nego (lengths consistent)
V_COOKIE_BAD='\x03\x00\x00\x13\x0e\xe0\x00\x00\x00\x00\x00Cookie: mstshash=x\x01\x00\x08\x00\x03\x00\x00\x00'      # cookie jammed in, declared length TOO SHORT (mismatch)

NEGO_VARIANTS="V_BADTYPE V_P00 V_P01 V_P02 V_P04 V_P08 V_P0B V_PFF V_NLEN0 V_NLENFF V_LIFF V_LI00 V_COOKIE V_COOKIE_BAD"

# --- Phase B variants: TPKT framing / length lies -----------------------------
V_LENFFFF='\x03\x00\xff\xff\x0e\xe0\x00\x00\x00\x00\x00'                                    # claims 65535, sends 11 then holds
V_LEN04='\x03\x00\x00\x04'                                                                  # claims 4 (header only, no body)
V_LEN08_LONG='\x03\x00\x00\x08\x0e\xe0\x00\x00\x00\x00\x00\x01\x00\x08\x00\x03\x00\x00\x00' # claims 8, sends 19 (extra bytes)
V_VER4='\x04\x00\x00\x13\x0e\xe0\x00\x00\x00\x00\x00\x01\x00\x08\x00\x03\x00\x00\x00'        # TPKT version 4 (must be 3)
V_VER0='\x00\x00\x00\x13\x0e\xe0\x00\x00\x00\x00\x00\x01\x00\x08\x00\x03\x00\x00\x00'        # TPKT version 0
V_LEN0000='\x03\x00\x00\x00'                                                                # TPKT length 0

# =============================================================================
say "PHASE A — X.224 negotiation-request fuzzer (14 malformed rdpNegReq variants)"
if [ "$LOOPBACK" -eq 1 ]; then
  echo "  loopback => guard EXEMPT, so every variant reaches the parser (ideal for decoder coverage)."
else
  echo "  NON-loopback => the guard locks out after ~5 consecutive fails, so only the first few"
  echo "  land on the parser this round. For full decoder coverage, also run against 127.0.0.1."
fi
echo "  expect: each malformed CR is rejected cleanly (accept_begin failed) — NEVER a decoder panic."
for pass in 1 2; do
  for v in $NEGO_VARIANTS; do fire "${!v}" 0; sleep 0.25; done
done
echo "  28 malformed-negotiation attempts fired (14 variants x2)."

say "PHASE B — TPKT framing / length-lie fuzzer"
echo "  expect: the framing reader bounds a lying length and rejects — no unbounded wait, no panic."
fire "$V_LENFFFF" 3      # claims 65535, sends 11 — held 3s so we don't wedge our own test
fire "$V_LEN04" 0
fire "$V_LEN08_LONG" 0
fire "$V_VER4" 0
fire "$V_VER0" 0
fire "$V_LEN0000" 0
# a framing-level slowloris: claim huge, then just sit (bounded hold)
fire "$V_LENFFFF" 4 &
fire "$V_LENFFFF" 4 &
wait
echo "  8 framing-abuse attempts fired."

# =============================================================================
say "PHASE C — guard lockout-escalation walk (STEPS=$STEPS)"
if [ "$LOOPBACK" -eq 1 ]; then
  echo "  SKIPPED on loopback (the guard is loopback-exempt — it can't lock out 127.0.0.1)."
  echo "  Re-run against the server's LAN/overlay IP to exercise this: ./soak_abuse3.sh <ip> $PORT"
else
  echo "  tripping the initial lockout (6 quick fails > the 5-consecutive threshold)..."
  for i in $(seq 1 6); do fire "$V_BADTYPE" 0 & done; wait
  cool=30
  for s in $(seq 1 "$STEPS"); do
    echo "  step $s: ~${cool}s lockout expected; waiting it out, then 1 fail to escalate..."
    sleep $((cool + 3))
    fire "$V_BADTYPE" 0
    cool=$((cool * 2))
  done
  echo "  escalation walk done — expected cooldowns: 30s, 60s, 120s, ... (doubling, capped ~900s)."
  echo "  CONFIRM on the server:  grep 'macrdp::audit' ~/Library/Logs/macrdp.log | grep lockout"
  echo "  -> retry_after_secs should climb per step; a later CLEAN session resets the IP to 0."
fi

# =============================================================================
say "PHASE D — per-IP isolation (needs arg 3 = a second address of the same server)"
if [ -z "$ALT" ] || [ "$LOOPBACK" -eq 1 ]; then
  echo "  SKIPPED (no alt_host given, or running on loopback)."
  echo "  To run: ./soak_abuse3.sh <lan-ip> $PORT <overlay-ip>  — floods the first address to lock"
  echo "  THIS source IP, then connects the second (different source IP) to prove it's unaffected."
else
  echo "  flooding $HOST to lock this source IP..."
  for i in $(seq 1 8); do fire "$V_BADTYPE" 0 & done; wait
  echo "  now a valid CR to the alternate address $ALT (different source IP on our side)..."
  fire_to "$ALT" "$CR" 0
  echo "  CONFIRM on the server: the audit log should show a lockout for the FIRST peer IP but"
  echo "  the connection from the SECOND peer IP reaching the handshake (not rejected) — per-IP state."
fi

say "DONE — decoder-fuzz + guard-edge abuse complete"
echo "  Snapshot the server: pid unchanged? panics=0? RSS/FD back to baseline? guard engaged?"
echo "  Decoder win = every malformed CR logged 'accept_begin failed' with ZERO panics in macrdp.log."
