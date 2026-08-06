#!/bin/bash
# soak_abuse4.sh — DEFENSIVE resilience self-test for macrdp (this project's own
# RDP server): the UDP MULTITRANSPORT surface (MS-RDPEUDP / MS-RDPEMT), the
# companion to the TCP-only soak_abuse{,2,3}.sh. Those three never touch UDP.
#
# When multitransport is on — which --enable-lossy-audio also IMPLIES — macrdp
# binds a UDP listener on the same address/port as TCP and parses RDPEUDP
# framing (later RDPEMT/DTLS) from an UNAUTHENTICATED peer BEFORE any session
# binds. That is a second decoder surface AND a connectionless per-peer map
# (HashMap<SocketAddr, Peer>) that no TCP test can reach:
#
#   PHASE A — malformed RDPEUDP framing (garbage / sub-header / oversized / length-lie)
#   PHASE B — SYN-family decode stress (SYN, SYN_LOSSY, SYNEX, all-flags + junk payloads)
#   PHASE C — peer-map growth flood (SYN from MANY source ports → one Peer entry each;
#             confirms the ~60s idle-GC bounds it, no per-peer memory runaway)
#   PHASE D — mixed sustained flood
#
# ONLY run this against a macrdp server YOU operate. It is deliberately abusive.
# Bounded + safe by design: ORDINARY UDP socket sends (no raw-packet/SYN flood),
# capped counts, short holds, no disk fill, no unbounded forks.
#
# The UDP path does NOT pass through the TCP auth-guard, so LOOPBACK is fine —
# run it at 127.0.0.1:<port>. REQUIRES python3, and the server's UDP listener
# actually bound (multitransport / lossy-audio enabled; the script preflights it
# and warns if it is not — the fuzz then hits a closed port and is a no-op).
#
# Usage:  ./soak_abuse4.sh [host] [port]        (default 127.0.0.1 3390)

set -uo pipefail
HOST="${1:-127.0.0.1}"
PORT="${2:-3390}"
ulimit -n 2048 2>/dev/null || true   # headroom for PHASE C's many source-port sockets

say()  { printf '\n=== %s ===\n' "$*"; }
have() { command -v "$1" >/dev/null 2>&1; }
have python3 || { echo "python3 required" >&2; exit 1; }

# --- preflight: server present + UDP listener bound ---
# MACRDP_PID / MACRDP_LOG override the auto-detected process + log — needed when a
# SECOND macrdp (e.g. a scratch UDP instance) runs alongside another (the soak),
# so the checks target the right one.
PID="${MACRDP_PID:-$(pgrep -x macrdp | head -1)}"
[ -n "$PID" ] || { echo "no macrdp process running" >&2; exit 1; }
rss() { ps -o rss= -p "$PID" 2>/dev/null | tr -d ' '; }
LOG="${MACRDP_LOG:-$HOME/Library/Logs/macrdp.log}"
panics_before="$(grep -ci panic "$LOG" 2>/dev/null)"; panics_before="${panics_before:-0}"
rss_before="$(rss)"

# Use netstat, NOT lsof, to check the port: lsof hangs enumerating fds on a host
# with RDPDR NFS mounts (it stats every fd, incl. stuck NFS ones).
if ! netstat -an 2>/dev/null | grep -i udp | grep -q "\.$PORT "; then
  echo "WARNING: no UDP socket bound on :$PORT (netstat) — multitransport/lossy-audio may be OFF."
  echo "         The fuzz will hit a closed UDP port (no-op). Set ENABLE_UDP_MULTITRANSPORT=1"
  echo "         (or ENABLE_LOSSY_AUDIO=1) to exercise the UDP decoder. Continuing anyway."
fi
echo "target udp $HOST:$PORT | macrdp pid=$PID rss=${rss_before}KB panics=$panics_before"

# --- UDP packet crafter. RDPUDP_FEC_HEADER (MS-RDPEUDP 2.2.2.1) =
#     snSourceAck: BE u32 (0xFFFFFFFF in a SYN) + uReceiveWindowSize: BE u16 + uFlags: BE u16. ---
PY="$(mktemp -t soak4.XXXXXX).py"
trap 'rm -f "$PY"' EXIT
cat > "$PY" <<'PYEOF'
import os, sys, socket, struct, random
HOST, PORT, MODE = sys.argv[1], int(sys.argv[2]), sys.argv[3]
SYN, FIN, ACK, DATA, FEC, SYN_LOSSY, SYNEX = 0x1, 0x2, 0x4, 0x8, 0x10, 0x200, 0x1000

def hdr(flags, ack=0xFFFFFFFF, win=0x0400):
    return struct.pack('>IHH', ack & 0xFFFFFFFF, win & 0xFFFF, flags & 0xFFFF)  # 8-byte FEC header

def newsock():
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.setblocking(False)
    return s

def send1(s, data):
    try:
        s.sendto(data, (HOST, PORT))
        return 1
    except OSError:
        return 0

n = 0
if MODE == 'frame':
    s = newsock()
    pkts = [os.urandom(L) for L in (0, 1, 3, 7, 8, 9, 16, 64, 256, 1024)]  # garbage of each length (sub-header..payload)
    pkts += [
        hdr(SYN),                        # header-only SYN, no SYNDATA payload
        hdr(SYN) + os.urandom(4),        # SYN + truncated payload
        hdr(SYN) + os.urandom(20),       # SYN + junk payload
        hdr(0xFFFF),                     # every flag set, no payloads actually present
        hdr(0xFFFF) + os.urandom(40),    # every flag + junk (forces all optional-header parses)
        hdr(ACK | DATA | FEC),           # data/fec/ack claimed, none present (length-lie)
    ]
    for p in pkts:
        for _ in range(3):
            n += send1(s, p)
elif MODE == 'synstress':
    reps = int(sys.argv[4]); s = newsock()
    flagsets = [SYN, SYN | SYNEX, SYN_LOSSY, SYN | SYN_LOSSY, 0xFFFF, SYN | ACK | DATA]
    for _ in range(reps):
        payload = os.urandom(random.randint(0, 64))
        n += send1(s, hdr(random.choice(flagsets), random.getrandbits(32), random.getrandbits(16)) + payload)
elif MODE == 'flood':
    nports, per = int(sys.argv[4]), int(sys.argv[5])
    socks = []
    for _ in range(nports):
        try:
            socks.append(newsock())      # each auto-binds a distinct source port => one Peer entry
        except OSError:
            break                        # fd-limited; stop growing
    for s in socks:
        for _ in range(per):
            n += send1(s, hdr(SYN) + os.urandom(24))
    for s in socks:
        try: s.close()
        except OSError: pass
    sys.stderr.write("peers=%d\n" % len(socks))
elif MODE == 'oversized':
    reps = int(sys.argv[4]); s = newsock()
    for _ in range(reps):
        n += send1(s, hdr(random.choice([SYN, 0, 0xFFFF])) + os.urandom(random.randint(2000, 9000)))
print(n)
PYEOF

udp() { python3 "$PY" "$HOST" "$PORT" "$@"; }

say "PHASE A — malformed RDPEUDP framing (garbage / sub-header / all-flags / length-lie)"
echo "  expect: each datagram is classified + rejected cleanly (no crash, no spin)."
echo "  sent $(udp frame) datagrams."

say "PHASE B — SYN-family decode stress (SYN / SYN_LOSSY / SYNEX / all-flags + junk payloads)"
echo "  expect: Datagram::decode fails gracefully on every malformed SYN; no panic."
echo "  sent $(udp synstress 400) datagrams."

say "PHASE C — peer-map growth flood (SYN from up to 400 source ports x3)"
echo "  expect: HashMap<SocketAddr,Peer> grows BOUNDED; the ~60s idle-GC reaps it; no per-peer runaway."
rss_c0="$(rss)"
peers_line="$(udp flood 400 3 2>&1 1>/dev/null)"     # one flood; capture the "peers=N" count (stderr)
sleep 2; rss_c1="$(rss)"
echo "  flood from ${peers_line:-peers=?} source ports; RSS ${rss_c0}KB -> ${rss_c1}KB (delta $(( ${rss_c1:-0} - ${rss_c0:-0} ))KB)."

say "PHASE D — mixed sustained flood (5 rounds of frame + syn + oversized)"
for _ in 1 2 3 4 5; do
  udp frame >/dev/null 2>&1; udp synstress 100 >/dev/null 2>&1; udp oversized 20 >/dev/null 2>&1
done
echo "  5 mixed rounds released."

# --- post-checks ---
say "POST-CHECKS"
sleep 1
rss_after="$(rss)"
panics_after="$(grep -ci panic "$LOG" 2>/dev/null)"; panics_after="${panics_after:-0}"

if kill -0 "$PID" 2>/dev/null; then echo "  PID $PID still alive — server did not crash ✅"
else echo "  !! PID $PID is gone — server CRASHED/exited ❌"; fi
echo "  RSS ${rss_before}KB -> ${rss_after}KB (delta $(( ${rss_after:-0} - ${rss_before:-0} ))KB) — want bounded (peers GC over ~60s)"
if [ "${panics_after:-0}" -gt "${panics_before:-0}" ]; then echo "  !! panics $panics_before -> $panics_after — NEW PANIC ❌"
else echo "  no new panics (${panics_after}) ✅"; fi

# Liveness: the UDP listener shares the process with the TCP accept loop; if the
# UDP fuzz wedged the runtime, a plain TCP connect would fail too. (Loopback ⇒
# auth-guard exempt, so this connect isn't counted.)
if (exec 3<>"/dev/tcp/$HOST/$PORT") 2>/dev/null; then exec 3>&- 3<&-; echo "  TCP accept still alive on :$PORT ✅"
else echo "  !! TCP connect to :$PORT FAILED — accept loop may be wedged ❌"; fi

echo
echo "NOTES:"
echo "  • To WATCH the decoder handle these, run the server with"
echo "      RUST_LOG=ironrdp_server::multitransport=debug   (classify/decode traces)."
echo "  • For a full peer-GC observation, re-check RSS ~70s after PHASE C (idle timeout is 60s)."
echo "  • DEEPER (not covered here, needs a completed RDPEUDP handshake): MS-RDPEMT cookie"
echo "    forgery/replay against the one-time-use CookieRegistry, and DTLS ClientHello fuzz."
