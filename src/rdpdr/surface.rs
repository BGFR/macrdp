//! macOS Finder surface for RDPDR drive redirection (Phase 1c).
//!
//! On connect, the client's redirected drive root is enumerated via
//! [`RdpdrHandle::list_dir`] and mirrored into a temp folder
//! `/tmp/macrdp-rdpdr-<pid>/<drive>/`: subdirectories become empty folders and
//! files become **pre-sized placeholders**, each backed by an `NSFilePresenter`.
//! The folder is opened in Finder. When the user opens a file, Finder's
//! coordinated read fires `relinquishPresentedItemToReader:`, where we block and
//! stream the bytes via [`RdpdrHandle::read_to_file`] before letting Finder
//! proceed — the same on-demand model as the lazy Windows→Mac paste
//! (`file_promise_lazy`), and it reuses the same dedicated CFRunLoop thread
//! (`runloop_thread`) for the `NSFileCoordinator` add/remove calls.
//!
//! Read-only, top-level only for now (subdirectory contents are a follow-on —
//! they show as empty folders). Cleaned up when the connection's backend drops.

#![cfg(target_os = "macos")]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use block2::Block;
use ironrdp_server::RdpdrHandle;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{declare_class, msg_send_id, mutability, ClassType, DeclaredClass};
use objc2_foundation::{NSFileCoordinator, NSFilePresenter, NSOperationQueue, NSString, NSURL};
use tokio::runtime::Handle;
use tracing::{debug, info, warn};

use crate::runloop_thread;

/// Per-presenter fetch state, indexed by the numeric id stored in the
/// presenter's ivars (Obj-C objects can't carry Rust closures cleanly).
struct PresenterState {
    url: Retained<NSURL>,
    queue: Retained<NSOperationQueue>,
    dst: PathBuf,
    handle: RdpdrHandle,
    device_id: u32,
    /// Windows-style path on the redirected drive, e.g. `\readme.txt`.
    remote_path: String,
    rt: Handle,
    /// Set after the first successful materialization so a second coordinated
    /// read doesn't re-fetch.
    fetched: Mutex<bool>,
}

struct SendRetained<T>(Retained<T>);
// Safe: we only call documented-thread-safe accessors on these immutable
// Foundation objects, and the presenter add/remove happens on the runloop thread.
unsafe impl<T> Send for SendRetained<T> {}
unsafe impl<T> Sync for SendRetained<T> {}

static REGISTRY: OnceLock<Mutex<HashMap<u64, Arc<PresenterState>>>> = OnceLock::new();
static LIVE_PRESENTERS: OnceLock<Mutex<HashMap<u64, SendRetained<RdpdrPresenter>>>> =
    OnceLock::new();
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn registry() -> &'static Mutex<HashMap<u64, Arc<PresenterState>>> {
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}
fn live_presenters() -> &'static Mutex<HashMap<u64, SendRetained<RdpdrPresenter>>> {
    LIVE_PRESENTERS.get_or_init(|| Mutex::new(HashMap::new()))
}

declare_class!(
    /// `NSFilePresenter` subclass, one per redirected file. Holds only a numeric
    /// id in ivars; real state lives in [`REGISTRY`].
    struct RdpdrPresenter;

    unsafe impl ClassType for RdpdrPresenter {
        type Super = NSObject;
        type Mutability = mutability::InteriorMutable;
        const NAME: &'static str = "MacrdpRdpdrFilePresenter";
    }

    impl DeclaredClass for RdpdrPresenter {
        type Ivars = Ivars;
    }

    unsafe impl RdpdrPresenter {
        #[method_id(init)]
        fn init(this: Allocated<Self>) -> Option<Retained<Self>> {
            let this = this.set_ivars(Ivars { id: 0 });
            unsafe { msg_send_id![super(this), init] }
        }
    }

    unsafe impl NSFilePresenter for RdpdrPresenter {
        #[method_id(presentedItemURL)]
        #[allow(non_snake_case)]
        fn presentedItemURL(&self) -> Option<Retained<NSURL>> {
            let id = self.ivars().id;
            registry().lock().unwrap().get(&id).map(|s| s.url.clone())
        }

        #[method_id(presentedItemOperationQueue)]
        #[allow(non_snake_case)]
        fn presentedItemOperationQueue(&self) -> Retained<NSOperationQueue> {
            let id = self.ivars().id;
            registry()
                .lock()
                .unwrap()
                .get(&id)
                .map(|s| s.queue.clone())
                .unwrap_or_else(|| unsafe { NSOperationQueue::mainQueue() })
        }

        #[method(relinquishPresentedItemToReader:)]
        unsafe fn relinquish_to_reader(&self, reader: *mut Block<dyn Fn(*mut Block<dyn Fn()>)>) {
            let id = self.ivars().id;
            let Some(state) = registry().lock().unwrap().get(&id).cloned() else {
                if let Some(reader) = reader.as_ref() {
                    reader.call((std::ptr::null_mut(),));
                }
                return;
            };

            // Block this presenter's operation-queue thread while we stream the
            // file (Apple's coordination model allows arbitrarily long blocking
            // here; Finder shows native progress).
            let mut fetched = state.fetched.lock().unwrap();
            if !*fetched {
                debug!(id, path = %state.remote_path, "rdpdr surface: streaming on read");
                match state.rt.block_on(state.handle.read_to_file(
                    state.device_id,
                    &state.remote_path,
                    &state.dst,
                )) {
                    Ok(n) => {
                        *fetched = true;
                        info!(id, path = %state.remote_path, bytes = n, "rdpdr surface: file materialized");
                    }
                    Err(e) => {
                        warn!(id, path = %state.remote_path, error = %e, "rdpdr surface: read failed; removing temp file");
                        let _ = std::fs::remove_file(&state.dst);
                        registry().lock().unwrap().remove(&id);
                    }
                }
            }

            if let Some(reader) = reader.as_ref() {
                reader.call((std::ptr::null_mut(),));
            }
        }
    }
);

unsafe impl NSObjectProtocol for RdpdrPresenter {}

struct Ivars {
    id: u64,
}

/// A live Finder surface for one redirected drive. Dropping it removes the
/// presenters and deletes the temp folder.
#[derive(Debug)]
pub struct Surface {
    state: Arc<Mutex<SurfaceState>>,
}

#[derive(Default, Debug)]
struct SurfaceState {
    temp_dir: Option<PathBuf>,
    presenter_ids: Vec<u64>,
}

impl Surface {
    /// Build the surface for `device_id` (a redirected filesystem labelled
    /// `drive_label`): enumerate the root, mirror it into a temp folder, register
    /// presenters, and open it in Finder. Returns immediately; the build runs on
    /// the current tokio runtime. Must be called from an async (runtime) context.
    pub fn start(handle: RdpdrHandle, device_id: u32, drive_label: &str) -> Self {
        let state = Arc::new(Mutex::new(SurfaceState::default()));
        let rt = Handle::current();
        let label = sanitize_label(drive_label);
        let build_state = Arc::clone(&state);
        let build_rt = rt.clone();
        tokio::spawn(async move {
            if let Err(e) = build_surface(handle, device_id, &label, build_state, build_rt).await {
                warn!(device_id, error = %e, "rdpdr surface: build failed");
            }
        });
        Self { state }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        let (temp_dir, ids) = {
            let mut s = self.state.lock().unwrap();
            (s.temp_dir.take(), std::mem::take(&mut s.presenter_ids))
        };
        // Remove presenters on the runloop thread, then delete the temp dir.
        let drained: Vec<SendRetained<RdpdrPresenter>> = {
            let mut live = live_presenters().lock().unwrap();
            let mut reg = registry().lock().unwrap();
            ids.iter()
                .filter_map(|id| {
                    reg.remove(id);
                    live.remove(id)
                })
                .collect()
        };
        runloop_thread::submit(move || unsafe {
            for p in &drained {
                let proto: &ProtocolObject<dyn NSFilePresenter> = ProtocolObject::from_ref(&*p.0);
                NSFileCoordinator::removeFilePresenter(proto);
            }
        });
        if let Some(dir) = temp_dir {
            let _ = std::fs::remove_dir_all(&dir);
            debug!(?dir, "rdpdr surface: cleaned up temp dir");
        }
    }
}

async fn build_surface(
    handle: RdpdrHandle,
    device_id: u32,
    label: &str,
    state: Arc<Mutex<SurfaceState>>,
    rt: Handle,
) -> anyhow::Result<()> {
    let entries = handle.list_dir(device_id, "\\").await?;

    let root = std::env::temp_dir().join(format!("macrdp-rdpdr-{}", std::process::id()));
    let drive_dir = root.join(label);
    std::fs::create_dir_all(&drive_dir)?;
    state.lock().unwrap().temp_dir = Some(root.clone());

    let mut to_register: Vec<(u64, SendRetained<RdpdrPresenter>)> = Vec::new();
    for e in &entries {
        let dst = drive_dir.join(&e.name);
        if e.is_dir {
            // Show subdirectories as (empty) folders; lazy enumeration of their
            // contents is a follow-on.
            let _ = std::fs::create_dir_all(&dst);
            continue;
        }
        // Pre-size the placeholder so Finder shows the real size before fetch.
        if let Err(err) = std::fs::File::create(&dst).and_then(|f| f.set_len(e.size)) {
            warn!(?dst, "rdpdr surface: pre-allocate failed: {err}");
            continue;
        }
        let Some(dst_str) = dst.to_str() else {
            continue;
        };
        let url = unsafe { NSURL::fileURLWithPath(&NSString::from_str(dst_str)) };
        let queue = unsafe { NSOperationQueue::new() };
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        registry().lock().unwrap().insert(
            id,
            Arc::new(PresenterState {
                url,
                queue,
                dst,
                handle: handle.clone(),
                device_id,
                remote_path: format!("\\{}", e.name),
                rt: rt.clone(),
                fetched: Mutex::new(false),
            }),
        );
        let presenter: Retained<RdpdrPresenter> =
            unsafe { msg_send_id![RdpdrPresenter::class(), new] };
        unsafe {
            let ivars_ptr = (*presenter).ivars() as *const Ivars as *mut Ivars;
            (*ivars_ptr).id = id;
        }
        to_register.push((id, SendRetained(presenter)));
    }

    let ids: Vec<u64> = to_register.iter().map(|(id, _)| *id).collect();
    state.lock().unwrap().presenter_ids = ids;
    let count = to_register.len();

    runloop_thread::submit(move || {
        for (id, presenter) in to_register {
            unsafe {
                let proto: &ProtocolObject<dyn NSFilePresenter> =
                    ProtocolObject::from_ref(&*presenter.0);
                NSFileCoordinator::addFilePresenter(proto);
            }
            live_presenters().lock().unwrap().insert(id, presenter);
        }
    });

    info!(
        device_id,
        files = count,
        dir = ?drive_dir,
        "rdpdr surface: drive mirrored — opening in Finder"
    );
    // Open the folder in Finder.
    let _ = std::process::Command::new("/usr/bin/open")
        .arg(&drive_dir)
        .spawn();
    Ok(())
}

/// Make a drive label safe for a single path component (no `/`, no `:` etc.).
fn sanitize_label(label: &str) -> String {
    let trimmed = label.trim().trim_end_matches(':');
    let cleaned: String = trimmed
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c == ':' || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    if cleaned.is_empty() {
        "drive".to_owned()
    } else {
        cleaned
    }
}
