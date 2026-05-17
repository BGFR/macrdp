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
//! Phase 2a (this file's current state): the delegate class is declared
//! and `NSFilePromiseProvider` instances are placed on the pasteboard, but
//! `writePromiseToURL:completionHandler:` returns
//! `errSecUnimplemented`-style `NSError` instead of streaming bytes — so
//! a paste attempt fails cleanly instead of hanging. Phase 2b will wire
//! the actual cliprdr request/response loop in here.
//!
//! Threading model:
//! - `NSFilePromiseProviderDelegate::filePromiseProvider:fileNameForType:`
//!   carries a `MainThreadMarker`; Cocoa only calls it on the main thread.
//! - `writePromiseToURL:completionHandler:` runs on a background
//!   `NSOperationQueue` (by Cocoa default). The completion block must be
//!   invoked exactly once — failing to call it leaves Finder's paste
//!   dialog hanging indefinitely.
//!
//! Mutability: `InteriorMutable`, not `MainThreadOnly`, because the write
//! callback runs on a background thread.

#![cfg(target_os = "macos")]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use block2::Block;
use ironrdp_cliprdr::pdu::FileContentsResponse;
use objc2::rc::Retained;
use objc2::runtime::{NSObject, ProtocolObject};
use objc2::{declare_class, msg_send_id, mutability, ClassType, DeclaredClass};
use objc2_app_kit::{NSFilePromiseProvider, NSFilePromiseProviderDelegate, NSPasteboard};
use objc2_foundation::{NSArray, NSDictionary, NSError, NSObjectProtocol, NSString, NSURL};
use tokio::sync::oneshot;
use tracing::{debug, warn};

/// Routes `FileContentsResponse` PDUs back to whichever delegate task is
/// awaiting them, keyed by `stream_id`. One instance lives on
/// `MacCliprdr` (the factory) and is cloned into every backend and every
/// promise delegate. Phase 2a doesn't use it yet beyond construction;
/// Phase 2b will wire the registration / delivery.
#[derive(Clone, Debug, Default)]
pub struct DownloadRouter {
    pending: Arc<Mutex<HashMap<u32, oneshot::Sender<FileContentsResponse<'static>>>>>,
    next_stream_id: Arc<AtomicU32>,
}

impl DownloadRouter {
    /// Allocate a fresh stream-id and register the matching oneshot
    /// receiver. The id starts at 1 (mstsc treats 0 as "no stream") and
    /// wraps around `u32::MAX` -> 1 to keep zero out of circulation.
    #[allow(dead_code)] // Phase 2b
    pub fn register(&self) -> (u32, oneshot::Receiver<FileContentsResponse<'static>>) {
        let mut sid = self.next_stream_id.fetch_add(1, Ordering::Relaxed);
        if sid == 0 {
            sid = self.next_stream_id.fetch_add(1, Ordering::Relaxed);
        }
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(sid, tx);
        (sid, rx)
    }

    /// Called from the backend's `on_file_contents_response`: route a
    /// response to whichever task is awaiting this `stream_id`. If
    /// no one's waiting (e.g. the paste was cancelled before fulfillment),
    /// the response is dropped silently.
    pub fn deliver(&self, response: FileContentsResponse<'static>) {
        let stream_id = response.stream_id();
        if let Some(tx) = self.pending.lock().unwrap().remove(&stream_id) {
            let _ = tx.send(response);
        } else {
            debug!(stream_id, "no awaiter for FileContentsResponse; dropping");
        }
    }
}

/// Per-promise state owned by the `PromiseDelegate` instance. Each file
/// in the remote's FILEGROUPDESCRIPTORW gets its own delegate so the
/// state is naturally per-file rather than juggling indices through
/// `provider.userInfo()`.
pub struct PromiseIvars {
    /// Display name handed back from `fileNameForType:`.
    pub file_name: String,
    /// Index into the remote's file list — Phase 2b uses this as
    /// `FileContentsRequest.index` when fetching bytes.
    #[allow(dead_code)]
    pub file_index: i32,
    /// Total file size from the descriptor, or `None` for directories
    /// (which can't be byte-fetched). Phase 2b will skip the SIZE round
    /// trip when this is set and trust it as the streaming target length.
    #[allow(dead_code)]
    pub file_size: Option<u64>,
    /// Shared with the backend so responses route back here.
    #[allow(dead_code)]
    pub router: DownloadRouter,
}

declare_class!(
    pub struct PromiseDelegate;

    // SAFETY:
    // - Super is plain NSObject; no subclassing requirements.
    // - InteriorMutable (not MainThreadOnly) because
    //   `writePromiseToURL:completionHandler:` runs on a background
    //   NSOperationQueue. `fileNameForType:` still runs on the main
    //   thread (Cocoa guarantee via the MainThreadMarker parameter).
    // - PromiseDelegate has no Drop impl; ivars are POD/owned and drop
    //   cleanly via the auto-generated finalize.
    unsafe impl ClassType for PromiseDelegate {
        type Super = NSObject;
        type Mutability = mutability::InteriorMutable;
        const NAME: &'static str = "MacrdpFilePromiseDelegate";
    }

    impl DeclaredClass for PromiseDelegate {
        type Ivars = PromiseIvars;
    }

    unsafe impl NSObjectProtocol for PromiseDelegate {}

    unsafe impl NSFilePromiseProviderDelegate for PromiseDelegate {
        // NSFilePromiseProviderDelegate's `fileNameForType:` selector has
        // two ObjC args; the `MainThreadMarker` parameter the protocol
        // exposes to *callers* is a Rust-level safety marker injected by
        // the safe-wrapper layer, not a real ObjC argument. The impl
        // must match the ObjC selector arity, so MTM is absent here.
        #[method_id(filePromiseProvider:fileNameForType:)]
        unsafe fn file_promise_provider_file_name_for_type(
            &self,
            _provider: &NSFilePromiseProvider,
            _file_type: &NSString,
        ) -> Retained<NSString> {
            NSString::from_str(&self.ivars().file_name)
        }

        #[method(filePromiseProvider:writePromiseToURL:completionHandler:)]
        unsafe fn file_promise_provider_write_promise_to_url(
            &self,
            _provider: &NSFilePromiseProvider,
            url: &NSURL,
            completion_handler: &Block<dyn Fn(*mut NSError)>,
        ) {
            // Phase 2a: paste isn't wired to the cliprdr fetch yet. Fail
            // the promise immediately so Finder shows a "couldn't copy"
            // dialog rather than hanging.
            let path = url.path().map(|s| s.to_string()).unwrap_or_default();
            warn!(file = %self.ivars().file_name, dest = %path, "Phase 2a promise: not yet implemented");
            let domain = NSString::from_str("MacrdpFileClipboard");
            let err = NSError::errorWithDomain_code_userInfo(
                &domain,
                -1,
                Some(&NSDictionary::new()),
            );
            // SAFETY: blocks invoked from the same thread Cocoa called us
            // on. NSError pointer ownership is +0 (caller of the block is
            // responsible for retain/release semantics on Cocoa's side).
            completion_handler.call((Retained::as_ptr(&err) as *mut _,));
        }
    }
);

impl PromiseDelegate {
    /// Construct a delegate carrying the per-file context. `MainThreadMarker`
    /// isn't needed since we declared `InteriorMutable`, but we still go
    /// through the standard alloc + init dance.
    pub fn new(ivars: PromiseIvars) -> Retained<Self> {
        let this = Self::alloc().set_ivars(ivars);
        unsafe { msg_send_id![super(this), init] }
    }
}

/// Push N `NSFilePromiseProvider` objects onto the general pasteboard.
/// Existing pasteboard contents are cleared first, so this is a
/// single-shot "the remote just copied these files" advertisement.
///
/// `file_type_uti` is the UTI Cocoa hands back to `fileNameForType:`
/// callers — we default it to `"public.data"` (the catch-all binary type)
/// since the remote rarely tells us anything more specific than the file
/// name's extension. Finder happily accepts that and uses the basename
/// extension to display the right icon.
pub fn write_promises_to_pasteboard(delegates: &[Retained<PromiseDelegate>]) {
    if delegates.is_empty() {
        return;
    }
    let file_type = NSString::from_str("public.data");
    let providers: Vec<Retained<NSFilePromiseProvider>> = delegates
        .iter()
        .map(|d| unsafe {
            let proto = ProtocolObject::from_ref(d.as_ref());
            NSFilePromiseProvider::initWithFileType_delegate(
                NSFilePromiseProvider::alloc(),
                &file_type,
                proto,
            )
        })
        .collect();

    // NSPasteboard.writeObjects takes NSArray<NSPasteboardWriting>.
    // NSFilePromiseProvider conforms to NSPasteboardWriting (gated on the
    // NSPasteboard feature in objc2-app-kit, which we already enable).
    // from_vec is the constructor that accepts a Vec<Retained<T>> — needed
    // because ProtocolObject<dyn NSPasteboardWriting> isn't IsRetainable
    // (NSArray::from_slice wants &[&T] with T: IsRetainable, which only
    // holds for class-bound types, not arbitrary protocols).
    let writers: Vec<Retained<ProtocolObject<dyn objc2_app_kit::NSPasteboardWriting>>> = providers
        .into_iter()
        .map(ProtocolObject::from_retained)
        .collect();
    let array = NSArray::from_vec(writers);
    unsafe {
        let pb = NSPasteboard::generalPasteboard();
        pb.clearContents();
        let _wrote = pb.writeObjects(&array);
    }
    debug!(
        count = delegates.len(),
        "wrote file promises to NSPasteboard"
    );
}
