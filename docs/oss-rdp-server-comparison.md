# What macrdp does that other open-source RDP servers don't — verified

This is the evidence backing every "first" claim made elsewhere in the docs. It exists so
those claims are **citable, falsifiable, and re-verifiable** rather than folklore — and so
we don't overclaim.

**Verified 2026-07-20** by adversarial web research (106 agents; each candidate claim put
to a 3-vote refutation panel — 25 claims → 16 confirmed, 9 refuted) plus a direct read of
FreeRDP's source. The brief was explicitly to *disprove* the claims, not confirm them.

> **House rule: always write "as far as is known" / "first known" — never a bare "first"
> or "only".** These are negative-existence claims over a field that was not exhaustively
> enumerated (see [Limits](#limits-of-this-survey)). One claim we might have made was
> already false; assume the others could become false as upstreams move.

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

Related: [features.md](features.md) (the capability list),
[usb-redirection-feasibility.md](usb-redirection-feasibility.md),
[rdp-udp-multitransport-feasibility.md](rdp-udp-multitransport-feasibility.md),
[camera-extension-setup.md](camera-extension-setup.md).
