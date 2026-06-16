# vendor/ironrdp-acceptor — divergence log

Local fork of ironrdp-acceptor 0.8.0, copied 2026-06-12 from upstream master
(Devolutions/IronRDP@879ffed — the same rev the rest of the ironrdp git pins
use) and pulled in via `[patch.crates-io]` in the root Cargo.toml. Keep this
vendor dir until divergence (1) is upstreamed AND released.

(1) Honor the client's requested desktop size from Client Core Data (NOT
    upstreamed): `Acceptor` gains a `honor_client_desktop_size: bool`
    (default false, setter `set_honor_client_desktop_size`, carried across
    `new_deactivation_reactivation`). When set, the
    `BasicSettingsWaitInitial` state reads `gcc_blocks.core.desktop_width/
    desktop_height` — the resolution the client actually asked for (mstsc
    full-screen monitor size, FreeRDP `/size:WxH`) — and, if sane
    (200..=8192 per MS-RDPBCGR), replaces `self.desktop_size` AND patches
    the Bitmap capset in `server_capabilities` (same mutation
    `new_deactivation_reactivation` does) BEFORE the Demand Active is sent.
    The session is thereby negotiated at the client's resolution from the
    start with no deactivation-reactivation resize.

    Why the acceptor and not the server: the client's own requested size is
    ONLY visible in GCC Client Core Data, which upstream parses in
    `BasicSettingsWaitInitial` and discards (only `early_capability_flags`
    is kept). The Confirm Active bitmap capset — what
    `RdpServerDisplay::request_initial_size` receives — is no help:
    conformant clients (verified in FreeRDP's `rdp_apply_bitmap_capability_set`,
    and mstsc behaves the same) overwrite their desktop size with the
    server's Demand Active values and echo those back. And by the time the
    server sees ANY of this, the acceptor has already committed a size in
    Demand Active inside `accept_finalize`.

    Consumed by `vendor/ironrdp-server` divergence (9): `RdpServer::
    set_honor_client_desktop_size` forwards the flag to each connection's
    acceptor; macrdp wires it from the default-on client-resolution
    auto-adopt (`--no-client-resolution` opts out). The server's display
    handler still learns the adopted size through the normal
    `request_initial_size` call, because the client's Confirm Active echo
    now equals the adopted size.

    Upstream shape when offering this: likely the same flag + an
    `Acceptor::desktop_size()` getter, or a builder option; CBenoit may
    prefer exposing the raw core-data size in `AcceptorResult` instead and
    leaving adoption to the server crate — either works for macrdp, adapt
    on review.

(2) Expose the client's keyboard-layout id from Client Core Data (NOT
    upstreamed; added 2026-06-16): `Acceptor` gains a private
    `client_keyboard_layout: u32` (captured in `BasicSettingsWaitInitial`
    from `gcc_blocks.core.keyboard_layout`, alongside the size read in (1),
    but UNCONDITIONALLY — it's free and harmless), carried across
    `new_deactivation_reactivation`, and surfaced as a new
    `pub keyboard_layout: u32` field on `AcceptorResult` (0 = not sent).
    Consumed by `vendor/ironrdp-server` divergence (10), which publishes it
    to a shared cell macrdp's input handler reads to auto-select a non-US
    keyboard layout (`src/keyboard_layout.rs`). Purely additive (a new struct
    field + a new private field), so trivially upstreamable — offer alongside
    (1), since exposing core-data fields on `AcceptorResult` is the same shape
    CBenoit floated for the desktop size. See the keyboard-layout quirk note
    in docs/known-quirks.md.
