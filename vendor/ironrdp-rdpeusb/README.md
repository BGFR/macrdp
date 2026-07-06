# ironrdp-rdpeusb (vendored)

Vendored copy of [IronRDP](https://github.com/Devolutions/IronRDP)'s
`ironrdp-rdpeusb` (the MS-RDPEUSB / `URBDRC` PDU layer), copied from rev
`879ffed` — the same rev the root `Cargo.toml` pins the other ironrdp crates to.

Divergence from upstream: `SupportedUsbVer` is extended with the USB 3.x
versions and the decoder tolerates unknown versions, so a modern USB-3 device's
`ADD_DEVICE` capabilities parse instead of erroring. See `CLAUDE.md`.
