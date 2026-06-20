# Feedback Assistant request — `com.apple.developer.usb.host-controller-interface`

Draft text for requesting the managed entitlement needed to present a virtual USB
device on macOS via `IOUSBHostControllerInterface` / `AppleUSBUserHCI` — the
prerequisite for generic USB redirection (`MS-RDPEUSB`). See
[`docs/usb-redirection-feasibility.md`](usb-redirection-feasibility.md) for the
full feasibility writeup.

> **Status:** *not submitted.* This is a prepared draft to paste into Feedback
> Assistant if/when generic USB redirection is pursued. Fill in the bracketed
> fields first.

## Why this entitlement (one line)

`IOUSBHostControllerInterface` (the user-space virtual USB host controller) is
gated by the managed entitlement `com.apple.developer.usb.host-controller-interface`.
It is **not** in the Xcode Capabilities list, so it can't be self-enabled — it's
granted on request via Feedback Assistant, tied to your Team ID, then embedded in a
Developer ID provisioning profile. (This is a *different, more obtainable*
entitlement than the DriverKit `com.apple.developer.driverkit.transport.usb` that
blocks projects like `usbipd-mac`.)

## Feedback Assistant fields

| Field | Value |
|---|---|
| Platform | **macOS** |
| Descriptive Title | `Request for Entitlement — com.apple.developer.usb.host-controller-interface` |
| Problem Area | **USB** |
| Type of Feedback | **Other Bug** (Suggestion/Request) |

## Description (paste into "Describe the Issue")

> Hello — I'd like to request access to the managed entitlement
> **`com.apple.developer.usb.host-controller-interface`** for my team.
>
> **Team ID:** [your 10-char Team ID — find it via `codesign -dv` on any signed
> build, or developer.apple.com → Membership]
> **Developer:** [your name] *(individual Apple Developer Program account)*
>
> **Product:** *macrdp* — an open-source, native **RDP server for macOS**
> (functionally analogous to `xrdp` on Linux): RDP clients (Microsoft Remote
> Desktop / mstsc / FreeRDP) connect to a Mac and drive its desktop, with
> keyboard/mouse, display, audio, clipboard, drive redirection, and smart-card
> redirection. It is distributed as a **Developer ID-signed, notarized** app (not
> via the Mac App Store).
> Repository / project: https://github.com/clintcan/macrdp
>
> **Why I need the entitlement:** I'm adding **USB device redirection** (the RDP
> `MS-RDPEUSB` feature). When a connected client redirects one of its local USB
> devices, my server needs to **present that device as a locally-attached USB
> device on the Mac**, so standard macOS drivers/apps in the session can use it. I
> intend to implement this in user space using **`IOUSBHostControllerInterface`**
> (driving `AppleUSBUserHCI`) to stand up a virtual USB host controller and feed it
> the redirected device's descriptors and transfers — rather than shipping a kernel
> extension or a DriverKit `transport.usb` driver. This API path is what requires
> the `com.apple.developer.usb.host-controller-interface` entitlement.
>
> **Scope:** the entitlement would be embedded in a Developer ID provisioning
> profile and used only by this signed, notarized app. Happy to provide any
> additional detail about the use case.
>
> Thank you very much for considering the request.
>
> *[Your name / contact email]*

## Before you submit — customize

- **Marketing material:** the request format mentions linking marketing/company
  material; the **GitHub repo is a sufficient** product reference. Add a release
  page or README screenshot link if you have one.
- **Individual vs organization:** the draft says "individual account" — change if
  your team is an organization.
- **Honesty:** phrased as genuine intent to build the feature (matches the
  feasibility doc), explicitly noting Developer-ID/notarized distribution and the
  deliberate choice of the UserHCI path over kext/DriverKit. Specific, real use
  cases tend to get approved smoothly.

## After it's granted (next steps)

1. Create a **Developer ID provisioning profile** that includes the entitlement.
2. Add a provisioning-profile step to `packaging/make-app.sh` (today it signs with
   plain Developer ID and **no** profile).
3. Sign + notarize as usual; the feature then runs on a stock, SIP-on Mac.
4. Note: the entitlement is baked into *this* team's signature, so only the
   officially-signed build can use USB redirection (see the feasibility doc's
   "distribution wrinkle").
