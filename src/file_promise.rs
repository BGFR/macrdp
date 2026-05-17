//! Windows→Mac file clipboard fulfillment via `NSFilePromiseProvider`.
//!
//! When the Windows side advertises a `FileGroupDescriptorW` format and the
//! Mac user presses Cmd-V in Finder, we don't have the file bytes locally —
//! they're sitting on the remote machine and have to be fetched over the
//! RDP `CLIPRDR` channel as a series of `FileContentsRequest` PDUs. This
//! module wires that fetch into Cocoa's "promised file" mechanism so the
//! download is lazy: Finder only triggers the round-trip when the user
//! actually pastes.
//!
//! Today (Phase 1 of the implementation) only the scaffolding is here —
//! the delegate class is declared, but `writePromiseToURL` returns
//! "not yet implemented" via the completion handler so a paste fails
//! cleanly instead of hanging. Phase 2 will wire the actual streaming.
//!
//! Lifecycle and threading:
//! - `MacCliprdr` owns the delegate (so it survives across reconnects).
//! - Cocoa invokes `filePromiseProvider:fileNameForType:` on the main
//!   thread (the protocol's `MainThreadMarker` proves this).
//! - Cocoa invokes `writePromiseToURL:completionHandler:` on a background
//!   `NSOperationQueue` (any thread). The completion block must be invoked
//!   exactly once — failing to call it leaves Finder's paste dialog
//!   hanging indefinitely.

#![cfg(target_os = "macos")]

// All real implementation arrives in the next commit. This file currently
// just defines the placeholder so the rest of the wiring can compile and so
// `cargo check` exercises the cargo features needed for Phase 2.
