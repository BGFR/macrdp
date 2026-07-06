# vendor/ironrdp-rdpeusb — divergence log

Local fork of `ironrdp-rdpeusb` (the MS-RDPEUSB / `URBDRC` PDU layer), copied
2026-07-06 from Devolutions/IronRDP@`879ffed` — the same rev the root
`Cargo.toml` pins every other ironrdp crate to. Pulled in via
`[patch.crates-io] ironrdp-rdpeusb = { path = "vendor/ironrdp-rdpeusb" }`.

Upstream is `publish = false`, so it can't be a plain crates.io version dep; it
was previously a **git dep** in `vendor/ironrdp-server/Cargo.toml`. It's a **leaf
crate** (only the vendored `ironrdp-server` depends on it — nothing in the git
workspace does), so vendoring is a clean **one-sided** patch: the git dep became
`ironrdp-rdpeusb = "0.1"`, resolved by the root path redirect. Its own transitive
dep `ironrdp-str` (a git-workspace crate, never published) is pinned to the same
git rev via a root `[patch.crates-io] ironrdp-str = { git = … rev = 879ffed }`
entry (same shape as the `ironrdp-error` pin). Workspace-inherited `[package]`
fields (`edition`/`license`/…) and the `[lints] workspace` key are inlined /
dropped because we live outside the IronRDP cargo workspace (mirrors
`vendor/ironrdp-dvc`).

## Divergences

(1) **Lenient USB version/speed decode in `UsbDeviceCaps`** (`src/pdu/sink.rs`).
    Upstream decodes the four *device-reported* fields of the MS-RDPEUSB
    `USB_DEVICE_CAPABILITIES` (2.2.11) as strict enums and returns
    `unsupported_value_err!` on any value it doesn't name. In particular
    `SupportedUsbVer` (`bcdUSB`) only knew USB 1.0/1.1/2.0 (`0x100/0x110/0x200`),
    so a modern **USB-3 device** — whose `AddDevice` reports `0x320` (USB 3.2) —
    failed to decode. That decode error propagated out of the server's
    `DvcProcessor::process` and (before the vendored-server tolerance in
    divergence 16) **tore down the whole RDP session**; it still blocks reading
    the real descriptors. Fix: `SupportedUsbVer`, `UsbdiVer`, `UsbBusIfaceVer`,
    and `DeviceSpeed` are now data-carrying enums with the named values **plus an
    `Other(u32)` fallback** and `from_u32`/`to_u32` helpers (replacing the
    `#[repr(u32)]` + `as u32` encode), so an unknown value is preserved verbatim
    and never rejected. `SupportedUsbVer` additionally gains named `Usb30`/`Usb31`/
    `Usb32` (`0x300/0x310/0x320`) for readable logs. `NoAckIsochWriteJitterBufSize`
    likewise accepts any `u32` (was `0` or `10..=512`). The framing constants
    (`CbSize`, `HcdCapabilities`) stay strict — they validate the PDU layout, not
    device data. **Verified live** with a USB-3.2 flash drive over a
    FreeRDP-with-urbdrc client: `ADD_DEVICE` fully decodes (`usb_version=Usb32`),
    no error, session stays up.
    Upstreamable (the strict enums are a genuine interop bug against real Windows
    USB-3 devices) — offer it alongside the server-direction `UrbdrcServer` work.
    Keep this vendor dir until that lands AND releases.

    **Extension (2026-07-07, real mstsc): `AddDevice.usb_device == 0` accepted**
    (`src/pdu/sink.rs`). `AddDevice::decode` rejected the `UsbDevice` interface id
    in the reserved range `0x0..=0x3` (it assumed a device id is always `>= 0x4`,
    which holds for FreeRDP). But **real mstsc assigns `UsbDevice = 0`** to a
    redirected device — the device is addressed by its own per-device DVC, so a
    per-channel interface id of 0 is unambiguous and valid. The reject arm made
    every mstsc `ADD_DEVICE` fail to parse (`unsupported UsbDevice (0)`), so the
    device never reached the presenting side. Fix: accept the full `0x0..=0x3F_FF_FF_FF`
    plain-interface-id range (only the StreamId-masked high range stays invalid).
    FreeRDP (id `>= 0x4`) is unaffected. Verified live: an mstsc-redirected camera +
    audio/HID device parse and enumerate.

(2) **Interface-0 device-completion routing** (`src/pdu/mod.rs`,
    `UrbdrcClientPdu::decode`). Consequence of the mstsc `UsbDevice = 0` above: a
    URB completion for that device arrives on `interface_id == 0` (== `CAPABILITIES`),
    so the interface-id-first dispatch routed it to the capability-response decoder
    and failed with "invalid RIM_EXCHANGE_CAPABILITY_RESPONSE header" — i.e. macrdp's
    `GET_DESCRIPTOR` reply was silently dropped and the fetch hung until disconnect.
    Fix: the `CAPABILITIES` arm now, when the header is NOT a genuine caps response
    (`function_id.is_none() && mask == StreamIdNone`), falls through to a shared
    `decode_device_message(src, header)` that dispatches by `(function_id, mask)` —
    the same routine the `interface_id >= 0x4` arm uses. So `URB_COMPLETION` (0x101),
    `URB_COMPLETION_NO_DATA` (0x102), `IOCONTROL_COMPLETION` (0x100) and
    `QUERY_DEVICE_TEXT` responses all decode whether the device interface id is 0
    (mstsc) or `>= 0x4` (FreeRDP). NB `URB_COMPLETION` and `ADD_DEVICE` share
    function id `0x101` — they are disambiguated only by interface + mask, which is
    why this can't be a plain global function-id match and must stay keyed off the
    interface arm. Verified live: the mstsc device descriptor round-trips (real
    vid/pid read back). Upstreamable alongside (1) as a real-mstsc interop fix.
