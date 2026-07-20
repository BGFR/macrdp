# macrdp vs. other open-source RDP servers

Two things live here:

- **[Part 1 — Verified firsts](#part-1--verified-firsts)** — the evidence behind every "first"
  claim made elsewhere in the docs, so they are **citable, falsifiable, and re-verifiable**
  rather than folklore, and so we don't overclaim.
- **[Part 2 — Project comparisons](#part-2--project-comparisons)** — how macrdp actually stacks
  up against the other native macOS RDP servers, written adversarially (steelmanning theirs).

**They rot at different rates.** Part 1 is dated evidence about upstream *absences* and changes
slowly. Part 2 changes whenever either project ships something — re-read it with more suspicion.

**Verified 2026-07-20** by adversarial web research (106 agents; each candidate claim put
to a 3-vote refutation panel — 25 claims → 16 confirmed, 9 refuted) plus a direct read of
FreeRDP's source. The brief was explicitly to *disprove* the claims, not confirm them.

> **House rule: always write "as far as is known" / "first known" — never a bare "first"
> or "only".** These are negative-existence claims over a field that was not exhaustively
> enumerated (see [Limits](#limits-of-this-survey)). One claim we might have made was
> already false; assume the others could become false as upstreams move.

---

# Part 1 — Verified firsts

## Verdicts at a glance

| Capability (server direction) | Verdict | Confidence |
|---|---|---|
| **USB redirection** — present a client's USB device as a real local device (MS-RDPEUSB/URBDRC) | **First known** | High |
| **UDP multitransport** — actually carry channel data over UDP (MS-RDPEMT/RDPEUDP) | **First known** | High |
| **Camera redirection** — client webcam → **a real OS camera device**, end to end (MS-RDPECAM) | **First known** *(state it precisely — see below)* | High after source read |
| "First/only **native macOS** RDP server" | **❌ REFUTED — do not claim** | High |
| **H.264/EGFX** server-side encoding | **Not a first** — don't claim | High |
| Smart-card (MS-RDPESC) server direction; drive-as-a-real-mount | **Unadjudicated** — assert nothing | — |

---

## 1. USB redirection, server direction — first known

**Claim:** macrdp presents a *client-redirected physical USB device* as a real local device
on the server host (a flash drive mounts in Finder; a gamepad works).

**Evidence — structural, not just an open issue.** FreeRDP master's `channels/urbdrc/`
contains only `client/`, `common/`, `CMakeLists.txt`, `ChannelOptions.cmake` — **no
`server/` subdirectory**. Its `CMakeLists.txt` has `add_subdirectory(common)` and a
client block gated on `WITH_CLIENT_CHANNELS`, but **no `add_subdirectory(server)`** — so
server code doesn't live elsewhere either. Crucially, FreeRDP *does* ship
`channels/rdpecam/server/`, which proves this absence is meaningful rather than a
repo-layout artifact.

Corroborating: upstream issue [#7558](https://github.com/FreeRDP/FreeRDP/issues/7558)
("server side channel not implemented", opened 2022-01-15) is still **Open**; every 2026
URBDRC CVE is phrased strictly client-direction; and xrdp has explicitly **declined** to
implement it ([discussion #2673](https://github.com/neutrinolabs/xrdp/discussions/2673):
"unlikely to be something the project would want to take on the maintenance for").

**Caveat that softened "only" → "the only working one":**
[`zoa-kas/xrdp-usb-redirector`](https://github.com/zoa-kas/xrdp-usb-redirector) is a
vendored xrdp 0.9.21.1 fork with a single 2024-03-27 commit *"Add functionality for token
passing and USB device passthrough as RAW"* — 5 commits total, 0 stars, unmodified stock
xrdp README, dormant since 2024-11-20. **Its diff was never read**, and "as RAW" plus its
smart-card/token focus makes it unlikely to be MS-RDPEUSB per spec. It doesn't refute the
claim, but it's enough that "only" was too strong.

**Don't cite as counter-evidence:** the `CHANNEL_URBDRC_SERVER=ON` CMake option name or
the vestigial server `urbdrc.h` header on pub.freerdp.com — both exist without a server
implementation. Cite the source tree and build file.

## 2. UDP multitransport data path — first known

**Claim:** macrdp actually carries channel data (EGFX video; AAC audio on a lossy flow)
over an MS-RDPEMT tunnel on MS-RDPEUDP — not a TCP-side bootstrap stub.

**Evidence.** FreeRDP's client **hard-rejects** multitransport via a dedicated
`multitransport_no_udp` stub that unconditionally answers `E_ABORT`; core
`libfreerdp/core/multitransport.c` contains no RDPEUDP implementation. The RDPEUDP /
RDPEUDP2 work (David Fort) stayed **out of tree**. No surveyed OSS RDP server carries
channel data over UDP on either side.

Sources: [`multitransport.c`](https://github.com/FreeRDP/FreeRDP/blob/master/libfreerdp/core/multitransport.c),
[issue #10669](https://github.com/FreeRDP/FreeRDP/issues/10669),
[hardening-consulting UDP write-up](https://www.hardening-consulting.com/en/posts/20230109-udp-support-2.html).

## 3. Camera redirection — first known, but say it precisely

**⚠️ The imprecise version of this claim is false.** FreeRDP **does** ship server-direction
MS-RDPECAM code — `channels/rdpecam/server/` contains `camera_device_main.c` (~29.7 KB)
and `camera_device_enumerator_main.c` (~16.6 KB). So macrdp is **NOT** "the first OSS RDP
server to implement MS-RDPECAM server-side," and saying so invites an easy correction from
anyone who knows the tree.

**What that code actually does (read directly, 2026-07-20).** It is a *channel endpoint*,
not a pipeline. In `device_server_recv_sample_response()`:

```c
pdu.SampleSize = Stream_GetRemainingLength(s);
pdu.Sample     = Stream_Pointer(s);
IFCALLRET(context->SampleResponse, error, context, &pdu);
```

The payload is never processed — only its size and a pointer are extracted and handed to an
application-supplied callback. There is **no video decoding anywhere** (no ffmpeg/avcodec/
openh264) and **no OS device registration** (no V4L2 loopback or equivalent). The caller must
implement decode, presentation, and device exposure.

**So the defensible claim is the end-to-end path:** first known OSS RDP server to *decode*
the redirected samples and *register a real camera device with the host OS*, so ordinary
apps (Photo Booth, Zoom, FaceTime) can select it.

**Not a counterexample:** Apache Guacamole's RDPECAM work
([GUACAMOLE-1415](https://issues.apache.org/jira/browse/GUACAMOLE-1415)) is
client-direction — browser → guacd → Windows host. Despite the name, `guacd` acts
architecturally as an RDP *client*, so as a gateway it structurally cannot present a
redirected camera as a local OS camera.

## 4. ❌ "First native macOS RDP server" — REFUTED

**Do not make this claim.** Two independent projects predate this one:

| Project | Created | Stack |
|---|---|---|
| [x6nux/macrdp](https://github.com/x6nux/macrdp) | **2026-03-24** | GPL-3.0; a vendored/patched `ironrdp-server` — **the same lineage as this project**; H.264 + AVC444 via VideoToolbox, HiDPI, NLA |
| [CGKPK/RDPonMAC](https://github.com/CGKPK/RDPonMAC) | **2026-04-26** | Apache-2.0; libxrdp + ScreenCaptureKit; CGEvent/IOKit input; serves mstsc and sdl-freerdp |
| clintcan/macrdp (this project) | 2026-05-13 | Rust on IronRDP |

Both are genuine RDP servers (they terminate the protocol themselves — not VNC bridges or
proxies), and both are **earlier**. macrdp's docs never actually made this claim, so nothing
required retraction — it's recorded here so it's never made by accident.

**They do not threaten claims 1–3:** both are display + input only. Neither implements USB,
camera, UDP-multitransport, drive, or smart-card redirection.

→ Both projects are compared properly in **[Part 2](#part-2--project-comparisons)**.

## 5. What macrdp should NOT claim

- **H.264/EGFX server-side encoding is not a first** — xrdp and gnome-remote-desktop both do
  server-side H.264.
- **Smart-card (MS-RDPESC) server direction** and **drive redirection presented as a real
  filesystem mount** were **not adjudicated**. They may well be unusual, but assert nothing
  either way without a source-tree audit.
- [Lamco's comparison page](https://lamco.ai/comparison/) is marketing-quality; the
  verification panel rejected claims resting on it. Don't cite it in either direction.

## Limits of this survey

The honest boundary on claims 1–3: the survey did **not** affirmatively clear **ogon**,
**gnome-remote-desktop**, the **Weston/wlroots** RDP backends, **NeutrinoRDP**, or other
IronRDP-downstream servers (lamco, hypr, cosmic-ext, ARISU) for server-direction USB,
camera, or UDP. The claims rest on FreeRDP + xrdp absence-of-evidence — strong for those two
projects, but not an exhaustive field survey. Hence "as far as is known".

Also note that one supporting line of evidence was voted down during verification: two
claims asserting FreeRDP's merged MS-RDPECAM PR #10258 is client-only were **refuted**,
which is precisely why claim 3 was re-grounded on a direct source read rather than an
API-doc reading.

## Re-verifying this (it will rot)

These are absence claims about actively developed upstreams. To re-check:

1. **USB** — does `channels/urbdrc/` have a `server/` dir, or `add_subdirectory(server)` in
   its CMakeLists? Is [#7558](https://github.com/FreeRDP/FreeRDP/issues/7558) still open?
2. **UDP** — does `libfreerdp/core/multitransport.c` still answer `E_ABORT` via
   `multitransport_no_udp`? Has any RDPEUDP implementation landed in-tree?
3. **Camera** — does `channels/rdpecam/server/camera_device_main.c` still merely
   `IFCALLRET(context->SampleResponse, …)` with the raw payload, with no decoder and no OS
   device registration?
4. **Field** — have ogon / gnome-remote-desktop / the IronRDP downstreams grown any
   server-direction redirection channel?

**Part 2 rots faster than Part 1** and on a different trigger: Part 1 tracks *absences* in
upstreams that change slowly, while Part 2 tracks two actively-developed projects. Re-check
their READMEs and commit activity before repeating anything from it — particularly the
"where they're ahead" table, which is the part most likely to be out of date (and the part
we'd look worst getting wrong).

Related: [features.md](features.md) (the capability list),
[usb-redirection-feasibility.md](usb-redirection-feasibility.md),
[rdp-udp-multitransport-feasibility.md](rdp-udp-multitransport-feasibility.md),
[camera-extension-setup.md](camera-extension-setup.md).

---

# Part 2 — Project comparisons

## x6nux/macrdp — the closest peer

[`x6nux/macrdp`](https://github.com/x6nux/macrdp) deserves a real comparison rather than a
footnote: it is the **same architectural lineage** (a vendored, patched `ironrdp-server` +
`ironrdp-acceptor`, Rust, native macOS, VideoToolbox H.264) and it **predates this project
by ~7 weeks**. Confusingly, it has the same name. This section is written adversarially —
steelmanning theirs — because "we're better" is not a useful claim, and in several places
it isn't true.

**Facts (checked 2026-07-20):** created 2026-03-24, GPL-3.0, 23★/7 forks, 56 commits, last
*code* push 2026-05-18. Ours: created 2026-05-13, Apache-2.0, 17★, actively pushed.

### Where x6nux/macrdp is ahead of us — genuinely

| Their advantage | Our status |
|---|---|
| **AVC444 shipped** ("pixel-perfect color", RDP 10) | **We don't have it.** `src/avc444.rs` exists (YUV444 pack + benchmarks) but is **parked and unwired** — no CLI flag. They win on color fidelity for text/fine detail. |
| **Lock-screen capture** (CoreGraphics fallback when the screen is locked) | **We have no equivalent.** We only *document* that input to the login window is blocked. Their session survives a locked screen in a way ours does not. |
| **Polished GUI** — Dashboard with FPS chart, Statistics with bar charts, Console.app-style Logs, split-panel Settings | Ours is a **menu-bar controller** — functional, far less of an admin UI. Their last five commits were all UI work. |
| **TOML config with hot reload** | We use `config.env` and mostly **require a restart** (`launchctl kickstart -k`). |
| Earlier, more stars, simpler surface to audit | We're newer and much larger. |

If someone's need is *"let me see and drive my Mac's screen, with the best color"*, theirs is
a smaller, cleaner, arguably better-presented answer — and it has AVC444, which we shelved.

### Where macrdp is differentiated

The honest framing is that **these are different products**, not competing implementations of
the same one. Theirs is a focused *display + input* server. Everything below is absent from
their advertised feature set:

- **Audio.** RDPSND PCM + opt-in AAC. Theirs advertises **no audio at all** — for daily
  remote use that alone is decisive.
- **Clipboard**, including **bidirectional file copy** (folder trees, lazy paste). Not advertised by theirs.
- **Device redirection — the whole category:** USB (a redirected drive mounts in Finder, gamepads work),
  **camera** (client webcam → a real macOS camera), **drive** (client drive as a real read-write NFS volume),
  **smart card** (client's card usable by macOS PC/SC apps). Theirs has none.
- **Headless operation** — `CGVirtualDisplay` virtual displays plus `--capture-primary` /
  `--detach-primary` blanking, so the Mac serves a remote desktop without a monitor.
- **UDP multitransport** (MS-RDPEMT/RDPEUDP) with lossy-audio and adaptive bitrate.
- **Production hardening:** per-IP rate-limiting + escalating lockout, a structured JSON audit
  stream for SIEM, a health-check watchdog, bounded log rotation, mstsc blank-recovery, and
  RTT-aware rate control for VPN/high-latency links.
- **Input depth:** non-US keyboard layouts auto-detected from the client, optional Ctrl→Cmd
  remapping, symbolic-hotkey workarounds, an app-switcher HUD.
- **Licensing:** Apache-2.0 vs their GPL-3.0 — materially different if you want to embed or
  ship commercially.

### Fair summary

Theirs is the better *minimal* macOS screen server today (AVC444, lock-screen capture, nicer
UI). Ours is a *remote-desktop platform* — audio, clipboard, files, USB, camera, smart cards,
headless, and the operational hardening to leave running. Neither supersedes the other.

**Two concrete things to steal:** AVC444 is already 90% built here and should be un-parked;
and lock-screen capture is a real gap worth closing.

## CGKPK/RDPonMAC — the other native macOS RDP server

[`CGKPK/RDPonMAC`](https://github.com/CGKPK/RDPonMAC) (created 2026-04-26, Apache-2.0) is a
genuine native macOS RDP server built on a different stack: **libxrdp + ScreenCaptureKit**,
with `CGDisplayCreateImage` login-screen fallback, `CGEvent`/IOKit HID input injection, and
verified service to both mstsc and sdl-freerdp. It terminates RDP itself — not a VNC bridge,
not a proxy.

It is **display + input only**: no audio, clipboard, or any redirection channel. Its one
notable capability macrdp lacks is the **login-screen capture fallback** — see the
capture-primary lock quirk in [known-quirks.md](known-quirks.md) for why that turns out to
matter less than it sounds (a remotely-visible lock screen still cannot be typed into).

Its activity was not tracked in detail; treat the above as a snapshot, not a current status.

## Scope of Part 2

Deliberately limited to the two projects that were actually examined. **No feature matrix
against xrdp / gnome-remote-desktop / ogon appears here on purpose** — Part 1's
[Limits](#limits-of-this-survey) records that those were never affirmatively cleared, and a
tidy comparison grid would imply verification that does not exist.
