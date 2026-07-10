#!/usr/bin/env bash
#
# Local integration test for the macrdp security AUDIT LOG writes.
#
# Drives two real CredSSP/NLA connections with sdl-freerdp (`+auth-only`, so no
# GUI window — it runs only the authentication exchange and exits) against a
# loopback macrdp instance, then asserts the JSON audit stream
# (--audit-file) recorded the right accept / auth / disconnect events with the
# correct outcomes, fields, and (src_ip, src_port) correlation.
#
# It needs NO real Mac password: --skip-auth makes macrdp validate CredSSP
# against a throwaway --username/--password, and loopback is guard-exempt so the
# wrong-password attempt can't trip a lockout. See docs/audit-log.md.
#
# Requirements: sdl-freerdp (brew install freerdp), a macrdp build (auto-built),
# python3. Loopback only. Exit 0 = PASS.
#
# Usage:  scripts/test-audit-log.sh            # port 3399
#         MACRDP_TEST_PORT=4000 scripts/test-audit-log.sh
set -u

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PORT="${MACRDP_TEST_PORT:-3399}"
USER_NAME="audittest"
GOOD_PW="s3cret-good"
BAD_PW="s3cret-WRONG"
WORK="$(mktemp -d)"
AUDIT="$WORK/audit.jsonl"
SRVLOG="$WORK/macrdp.log"

FRDP="$(command -v sdl-freerdp || command -v sdl3-freerdp || true)"
[[ -z "$FRDP" ]] && { echo "!! sdl-freerdp not found (brew install freerdp)"; exit 2; }

# Pick an existing build that actually supports --audit-file (a stale binary
# from before the feature would fail); otherwise build debug.
supports_audit() { "$1" --help 2>/dev/null | grep -q -- '--audit-file'; }
BIN=""
for cand in "$REPO/target/release/macrdp" "$REPO/target/debug/macrdp"; do
  [[ -x "$cand" ]] && supports_audit "$cand" && { BIN="$cand"; break; }
done
[[ -z "$BIN" ]] && { echo "== building macrdp (debug) =="; (cd "$REPO" && cargo build) || exit 2; BIN="$REPO/target/debug/macrdp"; }

cleanup() { [[ -n "${SRV_PID:-}" ]] && kill "$SRV_PID" 2>/dev/null; wait 2>/dev/null; rm -rf "$WORK"; }
trap cleanup EXIT

# Portable timeout (macOS ships no `timeout`): run a command, hard-kill after N
# seconds, and reap the watchdog quietly so no async "Terminated" line prints.
run_to() {
  local t=$1; shift
  "$@" & local p=$!
  { sleep "$t"; kill -9 "$p"; } >/dev/null 2>&1 & local w=$!
  wait "$p" 2>/dev/null; local rc=$?
  kill "$w" 2>/dev/null; wait "$w" 2>/dev/null
  return $rc
}

echo "== starting macrdp (loopback :$PORT, --skip-auth, audit -> audit.jsonl) =="
MACRDP_HEALTHCHECK=0 "$BIN" \
  --bind "127.0.0.1:$PORT" --skip-auth \
  --username "$USER_NAME" --password "$GOOD_PW" \
  --audit-file "$AUDIT" --cert-dir "$WORK/certs" \
  >"$SRVLOG" 2>&1 &
SRV_PID=$!

# Wait for the listener; fail fast (dumping the server log) if it never binds.
for _ in $(seq 1 40); do
  nc -z 127.0.0.1 "$PORT" 2>/dev/null && break
  kill -0 "$SRV_PID" 2>/dev/null || { echo "!! macrdp exited early:"; cat "$SRVLOG"; exit 1; }
  sleep 0.25
done
nc -z 127.0.0.1 "$PORT" 2>/dev/null || { echo "!! macrdp never listened:"; cat "$SRVLOG"; exit 1; }
echo "   listening (pid $SRV_PID)"

echo "== connection 1: CORRECT password (expect auth success) =="
run_to 20 "$FRDP" "/v:127.0.0.1:$PORT" "/u:$USER_NAME" "/p:$GOOD_PW" /cert:ignore +auth-only /log-level:ERROR >/dev/null 2>&1
echo "   done"

echo "== connection 2: WRONG password (expect auth did_not_complete) =="
run_to 20 "$FRDP" "/v:127.0.0.1:$PORT" "/u:$USER_NAME" "/p:$BAD_PW" /cert:ignore +auth-only /log-level:ERROR >/dev/null 2>&1
echo "   done"

sleep 1  # let the blocking audit writer flush the trailing disconnects

echo
echo "== audit events written =="
cat "$AUDIT"
echo
echo "== assertions =="
python3 - "$AUDIT" <<'PY'
import json, sys
rows = [json.loads(l) for l in open(sys.argv[1]) if l.strip()]
def ev(**kw): return [r for r in rows if all(r.get(k) == v for k, v in kw.items())]

fails = []
def check(name, ok):
    print(("  PASS " if ok else "  FAIL ") + name)
    if not ok: fails.append(name)

check("audit file is non-empty", bool(rows))
check("every row has schema_version=1 + event + src_ip",
      all(r.get("schema_version") == 1 and "event" in r and "src_ip" in r for r in rows))
check("target is macrdp::audit on every row", all(r.get("target") == "macrdp::audit" for r in rows))

acc   = ev(event="accept")
au_ok = ev(event="auth", outcome="success")
au_no = ev(event="auth", outcome="did_not_complete")
dis   = ev(event="disconnect")

check(">=2 accept events (one per connection)", len(acc) >= 2)
check(">=1 auth success (correct password)", len(au_ok) >= 1)
check(">=1 auth did_not_complete WITH a reason (wrong password)",
      len(au_no) >= 1 and all(r.get("reason") for r in au_no))
check("auth success is INFO, auth failure is WARN",
      all(r.get("level") == "INFO" for r in au_ok) and all(r.get("level") == "WARN" for r in au_no))
check("auth events carry src_ip + src_port",
      all("src_ip" in r and "src_port" in r for r in au_ok + au_no))
check("reject events (if any) carry src_ip but NOT src_port",
      all(("src_ip" in r and "src_port" not in r) for r in ev(event="reject")))
check(">=2 disconnect events", len(dis) >= 2)

def tup(r): return (r.get("src_ip"), r.get("src_port"))
corr = any(tup(a) in {tup(x) for x in acc} and tup(a) in {tup(x) for x in dis} for a in au_ok)
check("auth-success (src_ip,src_port) correlates to its accept AND disconnect", corr)

check("auth-failure reason is a clean single-line token (no control chars)",
      all(not any(ord(c) < 0x20 or ord(c) == 0x7f for c in (r.get("reason") or "")) for r in au_no))

print()
if fails:
    print(f"RESULT: FAIL ({len(fails)} check(s) failed)"); sys.exit(1)
print("RESULT: PASS — audit log writes verified end-to-end"); sys.exit(0)
PY
rc=$?
echo
[[ $rc -eq 0 ]] && echo "OVERALL: PASS" || echo "OVERALL: FAIL"
exit $rc
