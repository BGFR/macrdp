# vendor/ironrdp-rdpdr — divergence log

Local fork of ironrdp-rdpdr 0.5.0, copied 2026-06-16 from upstream
Devolutions/IronRDP@879ffed (the same rev as the other git pins) and pulled in
via `[patch.crates-io]` in the root `Cargo.toml`. Keep this vendor dir until
divergence (1) is upstreamed AND released.

Upstream `ironrdp-rdpdr` is **client-oriented**: `Rdpdr` is a
`SvcClientProcessor`, and the PDU `Encode`/`Decode` impls in `pdu::efs` only
cover the direction a *client* needs (encode client→server, decode
server→client). macrdp is the **server**, so it needs the opposite halves on a
few PDUs. The wire structs, field layouts, and constants are all upstream and
reused as-is; we only add the missing server-direction halves.

(1) Server-direction decode halves + accessors (NOT upstreamed):
    - `ClientNameRequest::decode` — server reads PAKID_CORE_CLIENT_NAME.
    - `ClientDeviceListAnnounce::decode` — server reads
      PAKID_CORE_DEVICELIST_ANNOUNCE (loops `DeviceAnnounceHeader::decode`).
    - `DeviceAnnounceHeader::decode` + `PreferredDosName::decode`, plus public
      accessors `device_id()` and `preferred_dos_name()`, and `device_type()`
      widened from `pub(crate)` to `pub`, so the server can read the announced
      device id / type / label.
    These pair with the **server-side `RdpdrServer` processor** that lives in
    `vendor/ironrdp-server/src/rdpdr.rs` (divergence (11) there) — kept out of
    this crate so the macrdp-facing factory/backend traits sit next to the other
    server channel factories. Outbound (server→client) reuses the existing
    `RdpdrPdu`/`*::encode` impls unchanged: `VersionAndIdPdu`, `CoreCapability`,
    `ServerDeviceAnnounceResponse` all have public fields and working `encode`,
    so the server constructs them directly.

    Phase 1b added the device-I/O halves: `encode` for `DeviceCreateRequest` /
    `DeviceReadRequest` / `DeviceCloseRequest` (write the `DeviceIoRequest`
    header + body), `decode` for `DeviceCreateResponse` / `DeviceReadResponse` /
    `DeviceCloseResponse`, and an `impl Encode + SvcEncode for
    ServerDriveIoRequest` (in `pdu/mod.rs`) that prepends the
    `PAKID_CORE_DEVICE_IOREQUEST` SharedHeader so the server can emit a request
    as an `SvcMessage`. Phase 1b-ii (list_dir) added `encode` for
    `ServerDriveQueryDirectoryRequest` and `decode` for `FileDirectoryInformation`
    (the directory-entry class the server requests); the query-directory response
    is decoded inline in the server's `RdpdrHandle::list_dir` (DeviceIoResponse +
    Length + one entry per response, looped until NO_MORE_FILES).

    Phase 2 (writes) added the server-direction halves for the write path:
    `encode` for `DeviceWriteRequest` and `ServerDriveSetInformationRequest`,
    `decode` for `DeviceWriteResponse`, `encode` for the set-information buffers
    `FileEndOfFileInformation` / `FileDispositionInformation` /
    `FileRenameInformation` / `FileAllocationInformation` (upstream had only
    their `decode`), the matching `FileInformationClass::encode` arms + a
    `FileInformationClass::level()` helper (maps a buffer to its
    `FileInformationClassLevel`), and the `DeviceWriteRequest` /
    `ServerDriveSetInformationRequest` arms in `ServerDriveIoRequest`'s
    `Encode`/`size` dispatch (`pdu/mod.rs`). `DeviceCreateRequest::encode`
    (Phase 1b) already carries the create-disposition, so create/mkdir reuse it.

    Upstreamable as a `SvcServerProcessor` peer to the client `Rdpdr` (offer the
    decode halves + the server processor together). De-vendor once a published
    ironrdp-rdpdr carries a server-side path.

Cargo notes: the de-worked `Cargo.toml` inlines the workspace-inherited fields
(edition 2024, rust-version, license, …) and drops the `path = "../ironrdp-*"`
deps, resolving them through the root `[patch.crates-io]` git pins — same shape
as `vendor/ironrdp-acceptor`. Its `ironrdp-error = "0.1"` dep is why the root
adds an `ironrdp-error` git pin to `[patch.crates-io]`: without it, this crate
would pull `ironrdp-error` from crates.io and split it from the copy the other
ironrdp crates use transitively.
