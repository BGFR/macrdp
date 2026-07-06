# Generic USB Redirection on macrdp — Feasibility Notes

*Research notes, 2026-06-20. Exploratory — macrdp does **not** implement generic
USB redirection today, and nothing here is committed work. This is a scoping
document for if/when it's ever pursued.*

> **UPDATE 2026-07-01 — Phase 1 is GO ✅, and two early assumptions below were wrong.**
> The entitlement `com.apple.developer.usb.host-controller-interface` was **granted** to
> team QGLA89KHM7 (FB23363880). A signed+provisioned spike (`--usb-spike`, `src/usb_redirect/`)
> successfully instantiated `IOUSBHostControllerInterface` and the kernel driver began the
> command exchange — so the entitlement functions and the UserHCI route is real.
> **Correction 1:** `IOUSBHostControllerInterface` is **NOT undocumented private SPI** — it's a
> **public, SDK-headered API** in the public `IOUSBHost.framework` (headers incl.
> `IOUSBHostControllerInterface.h` + the `IOUSBHostCI*StateMachine.h` set) with a complete
> example `main()`; its doc literally says it "create[s] synthetic USB devices." So the "private
> SPI, hard, moving target" and "VirtualHere binary forensics" framing below is superseded —
> it's documented-API FFI. **Correction 2:** upstream IronRDP now has an `ironrdp-rdpeusb` crate
> with the **complete bidirectional MS-RDPEUSB PDU layer** (client processor only), so the
> protocol side is no longer "unprecedented / from scratch" — we add a server processor on that.
> See the `project_usb_redirection_feasibility` memory for specifics.
>
> **UPDATE 2026-07-06 — Phases 2 and 3.0 are GO ✅ (branch `feat/usb-redirect-spike`).**
> - **Phase 2 (P2 below) DONE** (commit `ab91a63`): `src/usb_redirect/usb_spike.m` drives the
>   full UserHCI command/doorbell loop and a **hardcoded synthetic device enumerates LIVE in
>   `ioreg`** (VID 0x1209/PID 0x0001, complete EP0 GET_DESCRIPTOR flow, clean teardown) — the
>   whole macOS *presenting* path is proven end-to-end.
> - **Phase 3.0 (a go/no-go slice of P3) DONE** (commit `3a435c9`): a server-direction
>   `URBDRC` DVC **observe-only** processor (`--enable-usb-redirection`) advertises the channel
>   and runs the MS-RDPEUSB capability exchange. **Verified GREEN locally with a plain
>   `cargo build`** — the observe-only slice never touches the UserHCI controller, so **no
>   entitled build is needed for it** — via `sdl-freerdp /usb:auto`: the client opens URBDRC
>   (Create status 0) and completes the caps exchange (S_OK). (No `AddDevice` only because the
>   test Mac had no attachable USB device to redirect; channel-open + caps-exchange is the gate.)
>   Built as vendored `ironrdp-server` **divergence 16** (`src/rdpeusb.rs`), with
>   `ironrdp-rdpeusb` pulled in as a pinned-rev git dep (PDU-only — we drive the wire ourselves,
>   the same pattern as the server-direction RDPDR, rather than bump the whole IronRDP pin).
> - **Remaining: Phase 3.1** — the real forward: grow the processor into the handshake state
>   machine + an async `UsbHandle`/`UsbRouter` transfer path, and evolve `usb_spike.m` from
>   hardcoded+synchronous to client-sourced+async (the IOUSBHost-serial-queue ↔ tokio boundary).
>   Live 3.1 verification needs a physical redirectable device (or mstsc + the RemoteFX-USB
>   Group Policy). The *presenting* side still gates to the signed+provisioned entitled build.

## TL;DR

- **macrdp could, in principle, do generic USB redirection**, and the right macOS
  mechanism is a **user-space virtual USB host controller** — `IOUSBHostControllerInterface`
  driving Apple's built-in `AppleUSBUserHCI` provider ("UserHCI"). This is the same
  trick VirtualHere's client appears to use (per binary forensics — see below).
- The capability is **gated by a managed entitlement, `com.apple.developer.usb.host-controller-interface`**,
  which is **granted on request** (Feedback Assistant + Team ID) and is **not** the
  contentious DriverKit `transport.usb` entitlement that stalls `usbipd-mac`. So
  the permission is a *hurdle, not a hard wall*.
- The **real cost is the two big builds**: the RDP protocol side (`MS-RDPEUSB`,
  URB-level USB redirection — not in IronRDP, and server-side EUSB is essentially
  unprecedented) and the **UserHCI virtual-controller** implementation (private SPI,
  hard, a moving target).
- **Recommendation:** treat it as a large research project gated on a Phase-0
  entitlement-request spike. For most concrete needs, **device-class redirection**
  (drives ✅, smart cards ✅, printers/scanners/audio possible at the protocol
  layer) is dramatically less work and risk than generic USB.

## Context — what macrdp does today

macrdp redirects devices by **device class / protocol**, never by presenting raw
USB hardware:

- **Drive redirection** (RDPDR / MS-RDPEFS) → a real NFS mount.
- **Smart-card redirection** (RDPDR / MS-RDPESC) → a user-space PC/SC IFD handler.

Both ride a *protocol* (file ops, PC/SC APDUs) that macOS already exposes a
plug-in point for, so there is no virtual hardware to synthesize. **Generic USB
redirection is a different beast**: the client redirects an *arbitrary* USB device
(URB level), and the server has to make it appear as a real local USB device so any
macOS driver/app binds to it.

## The two layers any solution needs

In RDP, the **client** redirects its USB device to the **server** — so macrdp would
be the **presenting/consuming** side (the harder side, in VirtualHere terms).

1. **RDP protocol layer** — receive the redirected device + its USB traffic.
2. **macOS presentation layer** — present it as a local USB device and pump the
   traffic through.

## Layer 1 — the RDP protocol (`MS-RDPEUSB`)

RDP's generic USB redirection is **MS-RDPEUSB** (RemoteFX USB redirection): raw
**URB** (USB Request Block) forwarding over a **dynamic virtual channel** (DVC),
with device add/remove, channel setup, isoch/bulk/interrupt/control transfers, etc.

- **Server-direction is not in IronRDP** — this would be substantial vendoring,
  bigger than the RDPDR / RDPESC work already done. (Update 2026-06-25: IronRDP is
  adding *client*-side RDPEUSB — see [issue #1140 "[ironrdp-client] Wire up
  RDPEUSB with libusb backend"](https://github.com/Devolutions/IronRDP/issues/1140).
  That's the consuming/client direction; macrdp would still need the *server*
  direction, which remains absent. The client work could still be a useful PDU /
  URB-codec reference if it lands.)
- **Server-side EUSB is essentially unprecedented.** FreeRDP implements the *client*
  side (`urbdrc`); the server side is normally just Windows' own USB stack. macrdp
  would be charting new ground.

## Layer 2 — presenting the device on macOS (three options)

| Option | Mechanism | Status / cost |
|---|---|---|
| **Kext** | A kernel extension creating a virtual USB host controller (VirtualHere's *old* `vhhcd.kext`) | **Dead.** Deprecated; needs kext-signing or SIP off + reboot + user approval. Don't. |
| **DriverKit dext** | `USBDriverKit` System Extension | Needs the **restricted `com.apple.developer.driverkit.transport.usb`** entitlement — the wall **`usbipd-mac`** has been unable to get past. Heavy (System Extension, host app, approval). |
| **User-space virtual HCI** ⭐ | `IOUSBHostControllerInterface` → Apple's built-in `AppleUSBUserHCI` provider | **The viable route.** No third-party kext/dext. Needs the **grantable `com.apple.developer.usb.host-controller-interface`** entitlement. Private SPI, hard, moving target. |

### Why "user-space virtual HCI" is the one to use

A user-space process implements a USB **host controller interface** with
`IOUSBHostControllerInterface` + the `IOUSBHostCI*StateMachine` classes; it attaches
to Apple's **own** built-in `AppleUSBUserHCI` kernel provider, which registers the
synthetic device in the IORegistry so normal macOS drivers bind to it. The process
then feeds the controller with devices/endpoints backed by the remote (redirected)
device.

Important nuance: **it's "no *third-party* driver," not "no driver at all."** The
device is still registered by Apple's kernel HCI provider — it's just *driven from
user space*. That's the win: nothing to kext-sign, no dext entitlement gauntlet,
no SIP changes.

### Evidence VirtualHere takes this route

Binary forensics of VirtualHere's macOS client (shared via a reader, `nm`/`strings`):

- `IOUSBHostControllerInterface`, `IOUSBHostCIDeviceStateMachine`,
  `IOUSBHostCIEndpointStateMachine`, `IOUSBHostCIMessageTypeToString`
- `AppleUSBUserHCI…` / `…erHCIUserClient`
- `OSX11ClientDriver.mm`, `handleDeviceBind`/`Unbind`, "creating Host Controller",
  "virtual ports", `IOUSBHostDevice`

Those are the symbols of something *implementing* a host controller, not merely
*talking to* USB devices. (Inference from symbols, not confirmed runtime tracing —
the decisive confirmation would be a `ioreg -p IOUSB` before/after diff while a
redirected device is in use.)

## The entitlement situation (the make-or-break question)

- The UserHCI path requires the **managed** entitlement
  **`com.apple.developer.usb.host-controller-interface`**.
- It is **not** in the Xcode Capabilities panel (low request volume, never
  integrated into the portal), so it **can't be self-added** — Xcode will reject it
  if it isn't already in your provisioning profile.
- **You request it from Apple** via Feedback Assistant (macOS → Problem area "USB"),
  including your **Team ID**, a product overview, and any marketing links; per Apple
  forum guidance the approving engineer "is generally good about handling these
  quickly." No SIP-disable or root is mentioned.
- **This is a different, easier entitlement than the DriverKit one.** `usbipd-mac`'s
  blocker is `com.apple.developer.driverkit.transport.usb`; the UserHCI route needs
  `com.apple.developer.usb.host-controller-interface`, which appears genuinely
  obtainable. They are not the same wall.

## What it would take for macrdp (concretely)

*(Status tags added 2026-07-06; the two UPDATE blocks at the top of this file carry
the current picture — this list is the original scoping.)*

1. **[DONE ✅]** **Request + obtain** `com.apple.developer.usb.host-controller-interface`
   for the macrdp signing team. Granted to QGLA89KHM7 (FB23363880). The
   Feedback Assistant draft used is in [`docs/entitlement-request.md`](entitlement-request.md).
2. **[DONE ✅]** **Provisioning profile.** `packaging/make-app.sh` gained the profile step
   (`PROVISION_PROFILE=…`); the profile embedding the USB capability lives OUTSIDE the repo
   at `../provcerts/macrdp/macrdpprov2.provisionprofile` (a secret — never committed).
3. **[IN PROGRESS]** **Implement `MS-RDPEUSB` server-direction** in vendored IronRDP (URB DVC,
   device announce/remove, transfer types). **Phases 3.0 + 3.1a done** (divergence 16): the
   init handshake, the per-device DVC (opened via `ServerEvent::Urbdrc` →
   `DrdynvcServer::create_channel`), and the client's real `ADD_DEVICE` are all verified live.
   Remaining: **3.1b** — extend the caps decoder to parse USB-3 descriptors (vendor
   `ironrdp-rdpeusb`) + the async transfer path. Note: we write our own `UrbdrcServer` on the
   pinned `ironrdp-rdpeusb` **PDU layer** rather than adopt upstream's newer processor (which
   needs a breaking IronRDP pin bump).
4. **[IN PROGRESS — Phase 2 done]** **Implement the UserHCI virtual controller**
   (`src/usb_redirect/usb_spike.m`): **Phase 2 done** — a hardcoded synthetic device
   enumerates in `ioreg`. *(Correction: this is public IOUSBHost.framework API, NOT private
   SPI — see the top-of-file corrections; no `private_api.rs`-style boundary needed.)* Phase 3.1
   swaps the hardcoded descriptors + synchronous transfers for the client's real device over
   URBDRC (the async IOUSBHost-queue ↔ tokio boundary).
5. **[Phase 3.1+]** **Lifecycle**: device hotplug on client redirect, teardown on disconnect/exit.

## Distribution / packaging implications

- **Gated to the official signed build.** The entitlement is baked into *macrdp's*
  signature/profile. Ad-hoc CI artifacts and users building from source **could not**
  use USB redirection — only the build signed with the macrdp team's profile could.
  This breaks macrdp's "anyone can `cargo build` it and get every feature" property
  for this one feature (analogous in spirit to the smart-card USB-trigger caveat, but
  stronger — it gates the whole feature, not just deployment).
- Everything else (drives, smart cards, video, audio, clipboard) is unaffected.

## Recommendation

- **Gate everything on the entitlement spike first** (request it; cheap, and it's
  reportedly fast). If Apple declines for an RDP-server use case, stop — it's a hard
  wall and not worth the protocol work.
- If granted, scope the **MS-RDPEUSB + UserHCI** build as a genuine multi-week
  research project, with the UserHCI piece behind a private-API maintenance boundary.
- **Prefer device-class redirection when a specific class covers the need.** Generic
  USB only pays off for *arbitrary/custom* hardware nothing else can carry. Printers,
  scanners, audio, etc. each have their own RDP/protocol path that's far cheaper and
  carries no entitlement/SPI risk — the same "device-class beats generic USB" logic
  that made smart cards a user-space PC/SC handler instead of raw USB.

## Why this mirrors the smart-card decision

Smart cards rode a **question/answer protocol** (PC/SC) straight into a plug-in slot
macOS already exposes — no virtual hardware, no entitlement, no SPI. Generic USB has
no such slot, so it forces you down to *presenting hardware* — which is precisely the
heavy `IOUSBHostControllerInterface`/UserHCI + entitlement + protocol path above.
That contrast is the whole reason macrdp's redirection strategy is device-class-first.

## Sources

- Gist — creating virtual USB devices via `IOUSBHostControllerInterface` (states the
  required entitlement): <https://gist.github.com/JJTech0130/fae6b6ee6ae4232172a9188fb199d5d9>
- Apple Developer Forums — "is `com.apple.developer.usb.host-controller-interface`
  managed?" (how to request it): <https://developer.apple.com/forums/thread/802495>
- Apple Developer Forums — DriverKit `transport.usb` entitlement friction (the
  contrasting wall): <https://developer.apple.com/forums/thread/708501>
- `objc2-io-usb-host` crate (Rust bindings exist for the CI classes):
  <https://docs.rs/objc2-io-usb-host>
- VirtualHere (product): <https://www.virtualhere.com/>
- `usbipd-mac` (USB/IP for macOS, blocked on the DriverKit USB entitlement):
  <https://github.com/beriberikix/usbipd-mac>

## First open-source RDP *server* to present a redirected USB device

As far as is known, macrdp is the **first (and currently only) open-source RDP server
that receives a client-redirected USB device and presents it as a real local device** —
i.e. it implements the **server direction** of MS-RDPEUSB (`URBDRC`) plus local device
synthesis. This mirrors the project's earlier UDP-multitransport finding (first OSS RDP
server with a working UDP data path).

The claim is specifically about the *server/presenting* side. USB redirection in RDP is
inherently **client → server**: the client owns the physical device and redirects it; the
server must synthesize/present it. On real Windows RDS that presentation is done by
**closed-source kernel drivers** (`usbdr.sys` / the RemoteFX USB bus), not by any OSS
server.

**Verified 2026-07-06 against current sources:**
- **FreeRDP** — the most complete OSS RDP stack — implements `URBDRC` **client-direction
  only**. Its `channels/urbdrc/` tree has `client/` and `common/` subdirectories and **no
  `server/`** (`common/` is just the shared `msusb.c` PDU marshaling). FreeRDP issue
  [#7558 "server side channel not implemented"](https://github.com/FreeRDP/FreeRDP/issues/7558)
  documents this, and the project's own guidance states "the urbdrc channel has only the
  client side implemented." So FreeRDP-based servers (ogon, freerdp-shadow) cannot present a
  redirected device.
- **xrdp**, **ogon**, **gnome-remote-desktop** — no USB-redirection code at all (source-tree
  greps for `urbdrc`/`usbredir`/`usb_redir` returned nothing).
- **VirtualHere / usbip** present remote USB devices, but they are **USB-over-IP** (their own
  protocols), not RDP — and VirtualHere is proprietary.

Scope/hedge: "as far as is known" — this is a negative-existence claim over the OSS
landscape; it's backed by the source checks above, not a proof no niche project exists.
macrdp's presenting side is macOS-only (the UserHCI virtual host controller) and needs the
entitled/provisioned build.

## Status

**In progress — Phases 1, 2, 3.0, 3.1, and 3.2 (bulk/mount) done** (branch `feat/usb-redirect-spike`).
The entitlement `com.apple.developer.usb.host-controller-interface` is **granted**
(team QGLA89KHM7, FB23363880).
- **Phase 1 GO** — entitled build instantiates the `IOUSBHostControllerInterface`
  controller; kernel command exchange begins.
- **Phase 2 GO** — a hardcoded synthetic device enumerates live in `ioreg` (the whole
  macOS UserHCI presenting path proven).
- **Phase 3.0 GO** — the server-direction `URBDRC` DVC + MS-RDPEUSB init handshake
  (caps → CHANNEL_CREATED → RIMCALL_RELEASE) drives a real client to announce a device
  (`ADD_VIRTUAL_CHANNEL`), verified with a purpose-built FreeRDP-with-urbdrc client.
- **Phase 3.1a GO** — the server opens a **per-device DVC** on demand
  (`ServerEvent::Urbdrc` → `DrdynvcServer::create_channel`) and the client's real
  `ADD_DEVICE` (device descriptors) arrives on it (verified live, USB-3 flash drive).
  Both DVC `process()` impls tolerate decode errors so an unparseable PDU never tears
  down the session. Vendored `ironrdp-server` divergence 16.
- **Phase 3.1b(1) GO** — `ADD_DEVICE` now **fully parses** (real descriptors). The
  pinned `ironrdp-rdpeusb` `SupportedUsbVer` enum stopped at USB 2.0 and rejected a
  modern device's `0x320` (USB 3.2) caps, so `ironrdp-rdpeusb` is now **vendored** with
  a lenient `UsbDeviceCaps` decode (USB 3.x versions + `Other(u32)` fallbacks). Verified
  live with a USB-3.2 flash drive (`usb_version=Usb32`). See
  `vendor/ironrdp-rdpeusb/CLAUDE.md`.
- **Phase 3.1b(2a) GO** — a server-initiated **`GET_DESCRIPTOR` control transfer**
  round-trips real device data (proven observe-only, plain `cargo build`): on
  `ADD_DEVICE` the device processor sends `RegisterRequestCallback` +
  `TransferInRequest` and decodes the `URB_COMPLETION`. Verified live with a USB-3.2
  flash drive (`vid=0x2174 pid=0x2100`, read from the physical device). This de-risks
  the transfer path — libusb kernel-detach was not a blocker after unmount.
- **Phase 3.1b(2b) GO ✅✅ — a real client device enumerates locally** — the transfer
  path became a reusable async `UsbHandle`/`UsbRouter`, the driver moved into macrdp
  (`src/usb_redirect/mod.rs::drive_device`) via a `device_callback` seam, and
  `usb_spike.m` was restructured to async out-of-band EP0 completion. Verified entitled +
  FreeRDP: the client-redirected ESD310C flash drive enumerates on macrdp's UserHCI
  controller with descriptors/strings sourced live from the client. Controller is
  destroyed on disconnect (a `watch` channel → `closed()`), not leaked.
- **Phase 3.2 GO ✅✅ — the redirected USB DRIVE MOUNTS on the Mac** — `select_configuration`
  opens the device's pipe handles and `UsbHandle::bulk_transfer_in/out`
  (`TsUrb::BulkInterruptTransfer`) forward bulk on the mass-storage endpoints, so the
  macOS driver's SCSI (CBW/data/CSW) rides the client's real drive. **Verified end-to-end
  on a real Linux FreeRDP client** (UTM-QEMU Ubuntu + a USB-2.0 hub for a claimable
  interface): the ESD310C **mounts and stays mounted** (1300+ steady bulk transfers, no
  resets/timeouts). Two load-bearing fixes: dedup on the device's hardware identity
  (`VID:PID:bcdDevice`, not the client's per-announce instance id — FreeRDP double-announces
  one drive, and presenting both duels two virtual drives over the one device); and an
  Obj-C endpoint-object identity guard on completion (a device reset destroys+recreates the
  endpoint at the same key, leaving a pending transfer pointing into a freed ring). Remaining
  3.2: control-OUT forwarding (mass-storage reset / Clear-Feature), retract/hot-unplug,
  true multi-device.

Cross-reference the `docs/known-quirks.md` smart-card note (kext vs dext vs UserHCI
rationale) and `project_usb_redirection_feasibility` memory for the running log.
