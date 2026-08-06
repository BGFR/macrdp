# Pin-bump soak & resilience results — IronRDP `879ffed8` → `a5d1c682` (v0.9.5)

Evidence gathered before landing the IronRDP pin bump (PR #178, merged to `main`
as commit `4fa7c8f`, v0.9.5). The bump retired two vendored forks
(`ironrdp-async`, the `ironrdp-rdpeusb` crate) and harvested one divergence
(QOI). This is the record that it cleared the endurance-soak gate and the full
adversarial + functional suite. **No panics, no leaks, no crashes across any
run.**

## Build under test

| | |
|---|---|
| Branch / pin | `chore/pin-bump-a5d1c682` @ IronRDP `a5d1c682` (+133 commits over `879ffed8`) |
| Version | 0.9.5 (0.9.3 version string on the branch) |
| Host | Mac mini (Apple M1), macOS 26.5, headless |
| Install | entitled signed build at `/Applications/macrdp.app`, run by the LaunchAgent |
| Config | `--enable-h264 --enable-aac --enable-drive-redirection --adaptive-bitrate --bitrate 3` (daily-driver) |

## 1. Endurance soak

A single process sampled every 60 s (`~/macrdp-soak-sampler.sh` → RSS, CPU, fds,
estimated connections, panic count, restart count).

| Metric | Result |
|---|---|
| **Duration** | **~27.4 h continuous** (2026-08-05 07:22:07Z → 2026-08-06 10:52:46Z), 1646 samples |
| **Restarts during the run** | **0** (the process, pid 92675, never restarted) |
| **Panics** | **0** (sampler column and `macrdp.log` grep both zero) |
| **RSS** | avg **54 MB**, min 20 MB, max 116 MB (transient, during active H.264 sessions) |
| **RSS trend** | none — oscillates with load and returns to baseline; **no leak** |
| **Concurrent connections** | up to **2** (served real clients throughout) |

The run ended only because the process was deliberately restarted to reconfigure
it for the UDP resilience test (§2.4) — not by any fault. The 24 h soak gate was
cleared with ~3.4 h to spare.

## 2. Adversarial / abuse resilience

Four bounded, self-server abuse harnesses (`scripts/soak_abuse{,2,3,4}.sh`), each
fired at the live server and checked for survival (PID stable, no panic, no fd/mem
runaway, accept loop alive). All ran on the mini.

| Harness | Surface | Vectors | Result |
|---|---|---|---|
| `soak_abuse` | TCP, pre-TLS | rate-flood, malformed payloads, half-open holds, connection burst, sustained | **PASS** — PID stable, **no fd leak** (42→42), 0 panics |
| `soak_abuse2` | TCP handshake | slowloris (stall the TLS handshake), valid-CR-then-drop flood | **PASS** — no fd/mem pileup, 0 panics |
| `soak_abuse3` | TCP protocol decoder | X.224 / TPKT / fast-path malformed framing (length-lies) | **PASS** — every variant clean-rejects, no CPU spin |
| `soak_abuse4` | **UDP multitransport** (RDPEUDP / RDPEMT) | malformed RDPEUDP framing, SYN-decode stress, **400-source-port peer-map flood**, oversized / length-lie | **PASS** — no crash/panic, **peer-map bounded** (+~1 MB for 400 peers, idle-GC reclaims), TCP accept alive |

### 2.4 Notable: the `find_size` DoS, closed by the bump

`soak_abuse3` is the harness that originally **found** an unauthenticated,
pre-TLS remote DoS: a 2-byte fast-path frame drove `find_size` / `read_by_hint`
into a 100 % CPU non-yielding spin that also wedged the accept loop (whole-server
outage). On the **old** build it wedged the live server; on the **a5d1c682**
build it **clean-rejects** — upstream #1515 (`find_size` hardening, carried by the
pin) fixes it at the source. This soak was the confirmation that the pin closes
that hole.

### 2.5 Notable: UDP peer-map growth is bounded

`soak_abuse4` (new — the UDP companion to the three TCP harnesses) floods the
unauthenticated UDP listener with SYNs from up to **400 distinct source ports**,
one `HashMap<SocketAddr, Peer>` entry each. RSS grew only ~1–2.6 KB per peer
(~1 MB total) and the ~60 s idle-GC reclaimed it — no per-peer runaway, no leak.
Malformed RDPEUDP framing and oversized/length-lie datagrams were decoded and
rejected without a crash or spin.

## 3. Functional verification

Confirmed alongside the soak (see the pin-bump work log for detail):

- **FreeRDP** — CredSSP/NLA auth + the security audit stream, RDPSND **AAC**
  (upstream #1359, which the crate now owns — macrdp's `choose_audio_format`
  divergence retired), EGFX **H.264** (upstream #1345 frame-ack signature).
- **mstsc** — RemoteFX **USB redirection of an Xbox controller** end-to-end (the
  highest-risk area: the ported `src/rdpeusb.rs` div-16 URBDRC code against the
  strict client).
- **CI** (GitHub runner, canonical stable toolchain) — `test (linux)`,
  `test (macos)`, `audit log (macos integration)` (real CredSSP handshake, correct
  + wrong password), `cargo-deny` — **all green**.

## Conclusion

The pin bump cleared the endurance-soak gate (**27.4 h clean**, no panics / leaks
/ restarts), survived the full adversarial abuse suite including the new UDP
fuzz, passed functional verification on FreeRDP and mstsc, and went green on CI.
It landed to `main` as **`4fa7c8f`** (v0.9.5) via **PR #178**.

---

*Generated 2026-08-06. Environment: Mac mini (Apple M1); sampler
`~/macrdp-soak-samples.log`; harnesses `scripts/soak_abuse{,2,3,4}.sh`.*
