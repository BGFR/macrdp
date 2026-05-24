//! Dedicated CFRunLoop-hosting thread.
//!
//! macrdp's main thread is owned by `#[tokio::main]`, which doesn't pump
//! a CFRunLoop. Some Cocoa APIs — notably `NSFileCoordinator` /
//! `NSFilePresenter`, used by the lazy Windows→Mac file-paste POC — need
//! a live, pumped runloop to deliver their callbacks. This module owns a
//! dedicated `std::thread` whose only job is to run `CFRunLoopRun()`
//! forever, plus a `submit(closure)` API that trampolines arbitrary
//! `FnOnce() + Send + 'static` work onto that thread via a custom
//! `CFRunLoopSource`.
//!
//! The thread is spawned lazily on first `submit` call. There is no
//! shutdown path (matches the rest of macrdp — process exit reaps it).

#![cfg(target_os = "macos")]

use std::ffi::c_void;
use std::sync::mpsc::sync_channel;
use std::sync::{Mutex, OnceLock};
use std::thread;

use core_foundation::base::TCFType;
use core_foundation::runloop::{
    kCFRunLoopDefaultMode, CFRunLoop, CFRunLoopAddSource, CFRunLoopSourceContext,
    CFRunLoopSourceCreate, CFRunLoopSourceRef, CFRunLoopSourceSignal, CFRunLoopWakeUp,
};
// core-foundation re-exports kCFAllocatorDefault from core-foundation-sys::base::*
// via its own base module's `pub use`.
use core_foundation::base::kCFAllocatorDefault;
use tracing::warn;

type Job = Box<dyn FnOnce() + Send + 'static>;

/// The signal-source pointer is a Core Foundation object; CF objects are
/// safe to use from any thread (we only ever `CFRunLoopSourceSignal` it,
/// which Apple documents as thread-safe).
struct SafeSourceRef(CFRunLoopSourceRef);
unsafe impl Send for SafeSourceRef {}
unsafe impl Sync for SafeSourceRef {}

/// Likewise: `CFRunLoopWakeUp` is documented thread-safe.
struct SafeRunLoop(CFRunLoop);
unsafe impl Send for SafeRunLoop {}
unsafe impl Sync for SafeRunLoop {}

struct State {
    queue: Mutex<Vec<Job>>,
    source: SafeSourceRef,
    runloop: SafeRunLoop,
}

static STATE: OnceLock<&'static State> = OnceLock::new();

/// CFRunLoopSource perform callback — drains all queued jobs and runs them
/// on the runloop thread. Panics inside a job are caught so one bad job
/// can't kill the runloop.
extern "C" fn perform(_info: *const c_void) {
    let Some(state) = STATE.get() else { return };
    let drained: Vec<Job> = std::mem::take(&mut *state.queue.lock().unwrap());
    for job in drained {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
    }
}

fn ensure_started() -> &'static State {
    if let Some(s) = STATE.get() {
        return s;
    }
    // sync_channel(0) → rendezvous; we block until the worker thread has
    // initialized its runloop and source and reports back.
    let (tx, rx) = sync_channel::<&'static State>(0);
    thread::Builder::new()
        .name("macrdp-cfrunloop".into())
        .spawn(move || {
            let runloop = CFRunLoop::get_current();
            // CFRunLoopSourceContext is read by CF synchronously inside
            // CFRunLoopSourceCreate, so a stack-allocated context here is
            // fine — CF copies what it needs.
            let mut context = CFRunLoopSourceContext {
                version: 0,
                info: std::ptr::null_mut(),
                retain: None,
                release: None,
                copyDescription: None,
                equal: None,
                hash: None,
                schedule: None,
                cancel: None,
                perform,
            };
            let src_ref = unsafe { CFRunLoopSourceCreate(kCFAllocatorDefault, 0, &mut context) };
            assert!(!src_ref.is_null(), "CFRunLoopSourceCreate returned null");
            unsafe {
                CFRunLoopAddSource(
                    runloop.as_concrete_TypeRef(),
                    src_ref,
                    kCFRunLoopDefaultMode,
                );
            }
            // Leak the State — it lives for the process lifetime.
            let state: &'static State = Box::leak(Box::new(State {
                queue: Mutex::new(Vec::new()),
                source: SafeSourceRef(src_ref),
                runloop: SafeRunLoop(runloop),
            }));
            // Publish before running. If another thread races us by
            // calling submit() before STATE is set, ensure_started will
            // block on rx.recv() until we send.
            let _ = STATE.set(state);
            tx.send(state).expect("submit caller dropped");
            // Run forever. CFRunLoopRun returns only if every source/
            // timer/observer is removed or the runloop is stopped; we do
            // neither, so this never returns in practice.
            CFRunLoop::run_current();
            warn!("macrdp-cfrunloop thread exited unexpectedly");
        })
        .expect("spawn macrdp-cfrunloop thread");
    rx.recv().expect("cfrunloop thread initialization failed")
}

/// Run `f` on the dedicated CFRunLoop thread. Returns immediately —
/// `f` runs asynchronously the next time the runloop pumps.
pub fn submit<F: FnOnce() + Send + 'static>(f: F) {
    let state = ensure_started();
    state.queue.lock().unwrap().push(Box::new(f));
    unsafe {
        CFRunLoopSourceSignal(state.source.0);
        CFRunLoopWakeUp(state.runloop.0.as_concrete_TypeRef());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;
    use std::time::Duration;

    // One combined test so cargo's parallel test runner doesn't interleave
    // submits across tests onto the shared singleton runloop. The three
    // properties exercised: (1) closures run on the dedicated runloop
    // thread, (2) panicking closures don't kill the runloop, (3) ordering
    // within a single test is preserved.
    #[test]
    fn submit_basic_properties() {
        // (1) runs on the dedicated thread.
        let (tx, rx) = channel();
        submit(move || {
            tx.send(thread::current().name().map(|s| s.to_owned()))
                .unwrap();
        });
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap().as_deref(),
            Some("macrdp-cfrunloop")
        );

        // (2) a panicking job doesn't kill the runloop — a subsequent
        // submit still runs.
        submit(|| panic!("intentional"));
        let (tx, rx) = channel();
        submit(move || tx.send(42).unwrap());
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), 42);

        // (3) ordering within a single test's submits is preserved.
        let (tx, rx) = channel();
        for i in 0..20 {
            let tx = tx.clone();
            submit(move || tx.send(i).unwrap());
        }
        drop(tx);
        let mut got = Vec::new();
        while let Ok(v) = rx.recv_timeout(Duration::from_secs(2)) {
            got.push(v);
        }
        assert_eq!(got, (0..20).collect::<Vec<_>>());
    }
}
