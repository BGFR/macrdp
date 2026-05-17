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
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use block2::{Block, RcBlock};
use ironrdp_cliprdr::backend::ClipboardMessage;
use ironrdp_cliprdr::pdu::{FileContentsFlags, FileContentsRequest, FileContentsResponse};
use ironrdp_server::ServerEvent;
use objc2::rc::Retained;
use objc2::runtime::{NSObject, ProtocolObject};
use objc2::{declare_class, msg_send_id, mutability, ClassType, DeclaredClass};
use objc2_app_kit::{NSFilePromiseProvider, NSFilePromiseProviderDelegate, NSPasteboard};
use objc2_foundation::{NSArray, NSError, NSObjectProtocol, NSString, NSURL};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

/// Wraps an `RcBlock` so it can move across tokio's `spawn` boundary.
/// block2 conservatively marks blocks `!Send` because an arbitrary block
/// may capture thread-affine ObjC objects. Cocoa hands us
/// `NSFilePromiseProvider`'s completion block on a background
/// `NSOperationQueue` and documents that the block may be invoked from
/// any thread, so invoking it from a tokio worker is sound.
///
/// The inner pointer is held behind `Box::into_raw` so the closure
/// auto-trait analysis (which looks at captured *fields*, not whole
/// structs) cannot see the `NonNull<Block>` inside `RcBlock`. We only
/// invoke the block from one thread (the tokio worker that polls the
/// promise future), and the `Drop` impl reconstitutes the Box.
struct SendBlock {
    raw: *mut RcBlock<dyn Fn(*mut NSError)>,
}
// SAFETY: see above.
unsafe impl Send for SendBlock {}

impl SendBlock {
    fn new(block: RcBlock<dyn Fn(*mut NSError)>) -> Self {
        Self {
            raw: Box::into_raw(Box::new(block)),
        }
    }
    /// Invoke the block exactly once. Consumes self so a double-call is a
    /// type error.
    fn invoke(self, err: *mut NSError) {
        let boxed = unsafe { Box::from_raw(self.raw) };
        boxed.call((err,));
        // Manually forget self because Drop would double-free.
        std::mem::forget(self);
    }
}

impl Drop for SendBlock {
    fn drop(&mut self) {
        // Reached only if `invoke` was never called (e.g. spawned task
        // panicked before reaching the call site). Reclaim the box so the
        // RcBlock's underlying ObjC release runs.
        unsafe {
            let _ = Box::from_raw(self.raw);
        }
    }
}

/// Per-RANGE request size. mstsc / Microsoft Remote Desktop both seem to
/// happily handle 1 MiB chunks; smaller would wake up the response loop
/// more often, larger risks blowing past `MAX_FILE_RANGE_BYTES` on the
/// other side or hitting some peer-specific cap. 1 MiB matches what mstsc
/// itself asks for when fetching from us in the Mac→Windows direction.
const CHUNK_SIZE: u32 = 1024 * 1024;

/// Shared event sender used by both the cliprdr backend (for ack PDUs) and
/// any promise-fulfilling task. Holding `Option<...>` lets the factory
/// create the wiring before tokio hands it a real sender via
/// `ServerEventSender::set_sender`.
pub type EventSender = Arc<Mutex<Option<mpsc::UnboundedSender<ServerEvent>>>>;

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
#[derive(Clone)]
pub struct PromiseIvars {
    /// Display name handed back from `fileNameForType:`.
    pub file_name: String,
    /// Index into the remote's file list — used as
    /// `FileContentsRequest.index` when fetching bytes.
    pub file_index: i32,
    /// Total file size from the descriptor, or `None` if the remote
    /// didn't set it. When `None` we'll do a SIZE round-trip first; when
    /// set, we trust the value and skip straight to RANGE chunks.
    pub file_size: Option<u64>,
    /// Routes incoming `FileContentsResponse` back to the awaiting
    /// download task; cloned from `MacCliprdr`.
    pub router: DownloadRouter,
    /// Push `SendFileContentsRequest` events through the same channel the
    /// cliprdr backend uses for its own messages.
    pub sender: EventSender,
    /// Captured at delegate-construction time (inside a tokio context)
    /// so the Cocoa-thread `writePromiseToURL` callback can spawn back
    /// onto our runtime.
    pub rt_handle: tokio::runtime::Handle,
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
            // Top-of-function log so we can confirm Cocoa actually
            // invoked us. If a paste beeps and this line is missing, the
            // failure is upstream of our delegate (e.g. the
            // NSFilePromiseProvider died because nothing kept it alive).
            info!(file = %self.ivars().file_name, "writePromiseToURL entered");
            let dst: PathBuf = match url.path() {
                Some(p) => PathBuf::from(p.to_string()),
                None => {
                    warn!("file promise URL has no path; failing");
                    completion_handler.call((fail_error("bad URL"),));
                    return;
                }
            };
            let state = self.ivars().clone();
            // Retain the completion block so it outlives this synchronous
            // callback and can be invoked from the spawned task. Without
            // RcBlock::copy, the borrowed `&Block` becomes invalid the
            // moment we return. RcBlock::copy returns Option because the
            // underlying _Block_copy can theoretically fail under memory
            // pressure; in practice it never does, but we have to handle
            // the case — error the promise out so Finder doesn't hang.
            let completion = match RcBlock::copy(completion_handler as *const _ as *mut _) {
                Some(c) => SendBlock::new(c),
                None => {
                    completion_handler.call((fail_error("RcBlock::copy returned None"),));
                    return;
                }
            };
            let handle = state.rt_handle.clone();
            handle.spawn(async move {
                let err_ptr = match fetch_file(&state, &dst).await {
                    Ok(()) => {
                        info!(file = %state.file_name, dest = ?dst, "file promise fulfilled");
                        std::ptr::null_mut()
                    }
                    Err(e) => {
                        warn!(file = %state.file_name, dest = ?dst, "file promise failed: {e}");
                        // Best-effort: drop a partial file rather than
                        // leaving Finder with a half-written copy.
                        let _ = std::fs::remove_file(&dst);
                        fail_error(&e)
                    }
                };
                completion.invoke(err_ptr);
            });
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

/// Build a leaked-pointer `NSError` suitable for handing to the completion
/// handler. The block invokes Cocoa code that retains the error as it sees
/// fit, so we drop ownership here (`Retained::into_raw`).
///
/// userInfo is intentionally `None` — the heterogeneous
/// `NSDictionary<NSString, AnyObject>` shape Cocoa wants for a
/// `localizedDescription` value is awkward to build through objc2's
/// generics, and the message text is already in our tracing logs. The
/// domain + code is enough for Finder to show its generic
/// "couldn't paste" alert.
fn fail_error(msg: &str) -> *mut NSError {
    debug!("file promise error: {msg}");
    let domain = NSString::from_str("MacrdpFileClipboard");
    let err = unsafe { NSError::errorWithDomain_code_userInfo(&domain, -1, None) };
    Retained::into_raw(err)
}

/// Drive the full SIZE-then-RANGE download for a single promised file
/// and write the result to `dst`. Returns a human-readable error string
/// on any failure (channel closed, peer returned CB_RESPONSE_FAIL,
/// filesystem error, …) so the caller can package it into an `NSError`.
async fn fetch_file(state: &PromiseIvars, dst: &std::path::Path) -> Result<(), String> {
    // Prefer the descriptor-provided size; fall back to a SIZE round trip
    // if the remote left it blank.
    let total = match state.file_size {
        Some(s) => s,
        None => fetch_size(state).await?,
    };
    debug!(file = %state.file_name, total, "starting promise fetch");

    let mut file = std::fs::File::create(dst).map_err(|e| format!("create {dst:?}: {e}"))?;
    let mut position: u64 = 0;
    while position < total {
        let remaining = total - position;
        let req_size = remaining.min(u64::from(CHUNK_SIZE)) as u32;
        let chunk = fetch_range(state, position, req_size).await?;
        if chunk.is_empty() {
            // Remote returned no bytes — treat as premature EOF.
            return Err(format!(
                "short read at {position}; expected {req_size} bytes, got 0"
            ));
        }
        file.write_all(&chunk)
            .map_err(|e| format!("write {dst:?}: {e}"))?;
        position += chunk.len() as u64;
    }
    file.flush().map_err(|e| format!("flush {dst:?}: {e}"))?;
    Ok(())
}

async fn fetch_size(state: &PromiseIvars) -> Result<u64, String> {
    let (stream_id, rx) = state.router.register();
    push_request(
        &state.sender,
        FileContentsRequest {
            stream_id,
            index: state.file_index,
            flags: FileContentsFlags::SIZE,
            position: 0,
            requested_size: 8,
            data_id: None,
        },
    )?;
    let resp = rx.await.map_err(|_| "size: channel closed".to_string())?;
    if resp.is_error() {
        return Err("size: remote returned CB_RESPONSE_FAIL".into());
    }
    resp.data_as_size().map_err(|e| format!("size decode: {e}"))
}

async fn fetch_range(
    state: &PromiseIvars,
    position: u64,
    requested_size: u32,
) -> Result<Vec<u8>, String> {
    let (stream_id, rx) = state.router.register();
    push_request(
        &state.sender,
        FileContentsRequest {
            stream_id,
            index: state.file_index,
            flags: FileContentsFlags::RANGE,
            position,
            requested_size,
            data_id: None,
        },
    )?;
    let resp = rx.await.map_err(|_| "range: channel closed".to_string())?;
    if resp.is_error() {
        return Err(format!(
            "range at {position}: remote returned CB_RESPONSE_FAIL"
        ));
    }
    Ok(resp.data().to_vec())
}

fn push_request(sender: &EventSender, req: FileContentsRequest) -> Result<(), String> {
    let guard = sender.lock().unwrap();
    let s = guard
        .as_ref()
        .ok_or_else(|| "event sender unavailable (server shutting down?)".to_string())?;
    s.send(ServerEvent::Clipboard(
        ClipboardMessage::SendFileContentsRequest(req),
    ))
    .map_err(|_| "event channel closed".to_string())
}

/// Stash of `NSFilePromiseProvider` instances that must outlive the
/// `write_promises_to_pasteboard` call. NSPasteboard does **not** retain
/// promise writers for us — once the providers go out of scope, the
/// delegates die and Finder's later `writePromiseToURL` callback hits
/// dead memory, so Cocoa silently emits a paste-failure beep with no
/// log line on our side. Keeping them parked on the factory until the
/// next file list replaces them keeps the promises live.
///
/// SAFETY: `Retained<NSFilePromiseProvider>` is `!Send` by default
/// because ObjC `retain`/`release` aren't guaranteed atomic for every
/// class. NSPasteboard writers are an exception — they're retained by
/// Cocoa's cross-process pasteboard server with atomic refcounts. Our
/// Rust-side use is purely "hand to NSPasteboard once, hold until
/// replaced" with no method calls on the provider from outside Cocoa;
/// the `Mutex` serializes our replace operation. `Providers` wraps the
/// `Vec<Retained<_>>` with an `unsafe impl Send`/`Sync` to fit through
/// the `CliprdrBackend: Send` trait bound.
pub struct Providers(Vec<Retained<NSFilePromiseProvider>>);
// SAFETY: see ProviderStash doc.
unsafe impl Send for Providers {}
// SAFETY: see ProviderStash doc.
unsafe impl Sync for Providers {}

impl std::fmt::Debug for Providers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Providers({} entries)", self.0.len())
    }
}

pub type ProviderStash = Arc<Mutex<Providers>>;

/// Convenience constructor for an empty stash, used by factory init.
pub fn new_provider_stash() -> ProviderStash {
    Arc::new(Mutex::new(Providers(Vec::new())))
}

/// Push N `NSFilePromiseProvider` objects onto the general pasteboard
/// AND park them in `stash` so they survive long enough for Cocoa to
/// call back. Existing pasteboard contents are cleared first; the
/// previously-parked providers are dropped (they're stale now anyway).
///
/// `file_type` is the UTI Cocoa hands back to `fileNameForType:`
/// callers — we default it to `"public.data"` (the catch-all binary type)
/// since the remote rarely tells us anything more specific than the file
/// name's extension. Finder happily accepts that and uses the basename
/// extension to display the right icon.
pub fn write_promises_to_pasteboard(
    delegates: &[Retained<PromiseDelegate>],
    stash: &ProviderStash,
) {
    if delegates.is_empty() {
        stash.lock().unwrap().0.clear();
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
    let writers: Vec<Retained<ProtocolObject<dyn objc2_app_kit::NSPasteboardWriting>>> = providers
        .iter()
        .map(|p| ProtocolObject::from_retained(p.clone()))
        .collect();
    let array = NSArray::from_vec(writers);
    unsafe {
        let pb = NSPasteboard::generalPasteboard();
        pb.clearContents();
        let _wrote = pb.writeObjects(&array);
    }
    // Replace the stash AFTER the pasteboard write so old promises tied
    // to a previous remote copy drop in one swap.
    *stash.lock().unwrap() = Providers(providers);
    debug!(
        count = delegates.len(),
        "wrote file promises to NSPasteboard"
    );
}
