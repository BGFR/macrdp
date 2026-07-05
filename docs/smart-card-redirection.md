# Smart-card redirection

Use a smart card plugged into the connecting client from macOS apps
(`--enable-smartcard-redirection`) — setup, the USB-trigger caveat, and the
design rationale.

Opt-in with **`--enable-smartcard-redirection`** (off by default). The connecting
client redirects its **smart-card reader** and macOS PC/SC apps can use the card
through it — the standard RDP direction (MS-RDPESC), so the card stays on the
client while the Mac in the session reads it. Enable it on the client too
(mstsc: *Local Resources → More → Smart cards*; FreeRDP: `/smartcard`).

On the macOS side macrdp ships **its own PC/SC IFD handler** — a small reader
driver loaded by `com.apple.ifdreader` that presents the redirected card as a
real Finder/PC/SC reader and bridges every PC/SC call to the client over
MS-RDPESC. It's written from scratch (MIT/Apache), so there's **no GPL `vpcd`**
dependency. The whole chain is verified end-to-end on `mstsc` against a card,
including a full APDU transceive.

> **Why a user-space handler and not a kernel driver?** Redirection happens at
> the PC/SC (APDU) layer, not raw USB, and macOS's smart-card stack is user-space
> by design — the IFD handler is Apple's supported plug-in point, with no
> entitlements, signing gymnastics, or reboot a kext would demand. See the
> rationale in [docs/known-quirks.md](known-quirks.md).

<details>
<summary><b>In plain terms: why this "reader hook" instead of USB passthrough (à la VirtualHere)?</b></summary>

There are two ways to let a card plugged into the client be used by apps on the Mac:

- **Fake the hardware (the VirtualHere route).** Pretend the whole USB card-reader
  is physically plugged into the Mac. To make macOS believe a USB device is really
  attached, you write a low-level driver (a DriverKit *system extension*) — which
  needs Apple-granted permissions, a user-approved install, and a lot of plumbing
  to emulate the USB gadget. It's like **shipping the physical reader across the
  network and bolting a fake one onto the Mac's USB port.** Powerful and general
  (works for *any* USB gadget), but heavy.

- **Use the built-in slot (what macrdp does).** macOS already has a smart-card
  system (PC/SC) with an official plug-in slot for "reader helpers." macrdp drops in
  a tiny helper that says *"I'm a card reader,"* and whenever an app asks the card a
  question, the helper **forwards it over the network to the real card on the client
  and relays the answer back.** No fake USB device, no driver, no special
  permissions — it installs as a small file in a folder. Think of it as a
  **receptionist macOS already provides**, to whom we just hand a message-forwarder.

Smart cards talk a simple **question-and-answer protocol**, so we don't need to fake
any hardware — just pass the messages along, and macOS gives us the exact spot to
plug that in. The USB-passthrough approach is the right tool for sharing *arbitrary*
USB gadgets that have no such slot, but for smart cards it's massive overkill — all
that driver/permission friction to end up at the **same place** the small helper
reaches directly. Same result, far less machinery.

</details>

**One-time setup** — the IFD handler installs into a root-owned system directory,
so it can't be done by drag-to-Applications; run the bundled installer once (one
GUI admin prompt, no manual `sudo`):

```bash
# From a checkout, or from an installed app's Resources:
packaging/install-ifd-handler.sh
/Applications/macrdp.app/Contents/Resources/install-ifd-handler.sh   # DMG install

packaging/install-ifd-handler.sh --uninstall                          # remove
```

Run interactively, the installer **lists your attached USB devices and lets you
pick the one to use as the load trigger** (see the caveat below for why a trigger
is needed). To bind one non-interactively instead — or just to look up a device's
IDs — use the picker directly or pass them yourself:

```bash
packaging/select-usb-trigger.sh                       # list devices, print VID/PID
IFD_VID=0x2174 IFD_PID=0x2100 packaging/install-ifd-handler.sh   # bind explicitly
```

Then verify the reader registered with `system_profiler SPSmartCardsDataType`.

> macOS-only. **macOS loads a third-party IFD driver only on a USB *hotplug***
> matching the bundle's VID/PID, so a headless server needs a USB device
> permanently attached (any stick works as the trigger — pick it during install
> or bind it with `IFD_VID`/`IFD_PID`); after installing, unplug/replug it so
> `slotd` loads the driver. The handler talks to macrdp on loopback port 40242
> (`MACRDP_SCARD_PORT`). No physical card needed to try it: create a Windows
> **TPM virtual smart card** (`tpmvscmgr create …`) and redirect that.

> **Reloading after an upgrade.** `slotd` keeps the loaded handler in memory for
> its whole lifetime and ignores `SIGTERM`, so simply replacing the bundle on disk
> does nothing — a rebuilt or upgraded handler isn't picked up until `slotd` is
> killed with `SIGKILL` and the trigger device is replugged. Just **re-run the
> installer**: it restarts `slotd` correctly (`sudo pkill -9 -f com.apple.ifdreader`)
> and verifies the new bundle landed. Then unplug/replug the trigger so the fresh
> `slotd` loads the new driver. (If you ever do it by hand, note that
> `killall com.apple.ifdreader` won't match — the process name is truncated past
> 15 chars; use `pkill -9 -f`.)
