//! Windows→Mac file clipboard download + NSPasteboard publication.
//!
//! Originally tried via `NSFilePromiseProvider` so Finder could lazy-fetch
//! on paste, but Finder's Cmd-V does NOT invoke `NSFilePromiseReceiver`
//! (it's a drag-and-drop-only path); paste reads `NSPasteboardTypeFileURL`
//! directly and beeps when the URL is missing. So we eagerly download the
//! file bytes to a per-paste temp directory in `/tmp` the moment Windows
//! announces the copy, then publish the temp paths as real file URLs on
//! the pasteboard. Finder's paste then does a normal `cp` from /tmp to
//! the destination.
//!
//! Trade-offs vs. the promise approach:
//! - Network + disk happen eagerly even if the user never pastes. Cost is
//!   bounded by the cap in `clipboard.rs::MAX_INCOMING_PAYLOAD` analog
//!   (we don't enforce one yet — Phase 3 should add it).
//! - Finder paste is instantaneous once the temp file exists.
//! - Temp files outlive the paste; we clean them up on the next remote
//!   copy and on process exit. Stale files in `/tmp` are fine until then.
//!
//! Threading: download tasks run on the tokio runtime captured at
//! `on_remote_file_list` time. The `cocoa` helper in this module writes
//! to NSPasteboard from whichever thread the calling task is on —
//! NSPasteboard's API is documented thread-safe.

#![cfg(target_os = "macos")]

use std::collections::HashMap;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use ironrdp_cliprdr::backend::ClipboardMessage;
use ironrdp_cliprdr::pdu::{FileContentsFlags, FileContentsRequest, FileContentsResponse};
use ironrdp_server::ServerEvent;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_app_kit::NSPasteboard;
use objc2_foundation::{NSArray, NSString, NSURL};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

/// Per-RANGE request size. 1 MiB matches what mstsc itself asks for when
/// fetching files from us in the Mac→Windows direction. Larger chunks
/// (we tried 4 MiB) appeared to flood the SVC channel and starve the
/// display/audio path — the connection felt unresponsive while a big
/// download ran.
const CHUNK_SIZE: u32 = 1024 * 1024;

/// Maximum number of in-flight `FileContentsRequest` PDUs for a single
/// file. 8 × 1 MiB = 8 MiB of pending response data is enough to keep
/// the cliprdr round-trip pipelined on a LAN without head-of-line-
/// blocking other static virtual channels.
const MAX_PARALLEL_CHUNKS: usize = 8;

/// Shared event sender used by both the cliprdr backend (for ack PDUs) and
/// any eager-download task.
pub type EventSender = Arc<Mutex<Option<mpsc::UnboundedSender<ServerEvent>>>>;

/// Tracks the NSPasteboard `changeCount` value we ourselves set after
/// publishing remote files. The change-count poller in `clipboard.rs`
/// reads this and skips if the current changeCount matches — otherwise
/// the poller would see "new files on Mac pasteboard!" and round-trip
/// them back to Windows as a fresh Mac→Windows copy.
pub type SelfChangeCount = Arc<AtomicI64>;

/// Routes `FileContentsResponse` PDUs back to whichever download task is
/// awaiting them, keyed by `stream_id`. One instance lives on
/// `MacCliprdr` (the factory) and is cloned into the backend (which calls
/// `deliver` on each response) and into each spawned download task
/// (which calls `register` to allocate stream-ids).
#[derive(Clone, Debug, Default)]
pub struct DownloadRouter {
    pending: Arc<Mutex<HashMap<u32, oneshot::Sender<FileContentsResponse<'static>>>>>,
    next_stream_id: Arc<AtomicU32>,
}

impl DownloadRouter {
    /// Allocate a fresh stream-id and register the matching oneshot
    /// receiver. The id starts at 1 (mstsc treats 0 as "no stream") and
    /// skips zero on wrap-around.
    pub fn register(&self) -> (u32, oneshot::Receiver<FileContentsResponse<'static>>) {
        let mut sid = self.next_stream_id.fetch_add(1, Ordering::Relaxed);
        if sid == 0 {
            sid = self.next_stream_id.fetch_add(1, Ordering::Relaxed);
        }
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(sid, tx);
        (sid, rx)
    }

    /// Backend's `on_file_contents_response` calls this; routes the
    /// response to whichever task is awaiting this `stream_id`. Dropped
    /// silently if no one's waiting (e.g. paste was cancelled).
    pub fn deliver(&self, response: FileContentsResponse<'static>) {
        let stream_id = response.stream_id();
        if let Some(tx) = self.pending.lock().unwrap().remove(&stream_id) {
            let _ = tx.send(response);
        } else {
            debug!(stream_id, "no awaiter for FileContentsResponse; dropping");
        }
    }
}

/// One file in a remote-copy batch. The download task takes a `Vec` of
/// these and produces a `Vec<PathBuf>` of materialized temp paths.
#[derive(Clone, Debug)]
pub struct RemoteFile {
    pub index: i32,
    pub name: String,
    pub size: Option<u64>,
}

/// Spawned by `on_remote_file_list`: downloads every file in `files`
/// into a fresh temp directory, then publishes the resulting file URLs
/// to NSPasteboard. Records the resulting `changeCount` so the
/// Mac-side poller doesn't loop the paste back to Windows.
pub fn spawn_remote_paste(
    files: Vec<RemoteFile>,
    router: DownloadRouter,
    sender: EventSender,
    current_temp_dir: Arc<Mutex<Option<PathBuf>>>,
    self_change_count: SelfChangeCount,
    rt_handle: tokio::runtime::Handle,
) {
    if files.is_empty() {
        return;
    }
    rt_handle.spawn(async move {
        // Wipe any previous temp dir before starting the new download so
        // /tmp doesn't accumulate stale paste data across copies.
        if let Some(old) = current_temp_dir.lock().unwrap().take() {
            let _ = std::fs::remove_dir_all(&old);
        }
        let dir = match make_temp_dir() {
            Ok(d) => d,
            Err(e) => {
                warn!("failed to create paste temp dir: {e}");
                return;
            }
        };
        *current_temp_dir.lock().unwrap() = Some(dir.clone());

        let mut written: Vec<PathBuf> = Vec::with_capacity(files.len());
        for f in &files {
            let dst = dir.join(&f.name);
            match fetch_file(f, &dst, &router, &sender).await {
                Ok(()) => {
                    debug!(name = %f.name, dest = ?dst, "downloaded remote file");
                    written.push(dst);
                }
                Err(e) => {
                    warn!(name = %f.name, "download failed: {e}");
                    // Stop on first failure — a half-set on the
                    // pasteboard would confuse the user. Leave the temp
                    // dir behind for inspection; it'll get wiped on the
                    // next remote copy.
                    return;
                }
            }
        }
        publish_to_pasteboard(&written, &self_change_count);
        info!(
            count = written.len(),
            "published remote paste to NSPasteboard"
        );
        let summary = match written.as_slice() {
            [single] => single
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| format!("{n} ready to paste"))
                .unwrap_or_else(|| "File ready to paste".into()),
            many => format!("{} files ready to paste", many.len()),
        };
        notify_user(&summary);
    });
}

/// Fire a macOS desktop notification. Uses `osascript` because (a) it
/// avoids the entitlements / app-bundle dance that
/// `UNUserNotificationCenter` requires for an unbundled CLI and (b) it's
/// already on the system. Notifications appear attributed to "Script
/// Editor" in Notification Center settings — minor cosmetic quirk we
/// accept in exchange for not having to ship a `.app` bundle.
///
/// Failure is silent: if `osascript` is missing or the spawn fails we
/// still have the `published remote paste` log line and the pasteboard
/// content, so the worst case is the user has to glance at the terminal
/// instead of the desktop banner.
fn notify_user(message: &str) {
    // AppleScript string literals — escape `\` and `"`.
    let escaped = message.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!("display notification \"{escaped}\" with title \"macrdp\"");
    let _ = std::process::Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .spawn();
}

fn make_temp_dir() -> std::io::Result<PathBuf> {
    // Pid + nanos is unique enough for a single-process tool; we don't
    // need /dev/urandom for collision avoidance.
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("macrdp-paste-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn publish_to_pasteboard(paths: &[PathBuf], self_change_count: &SelfChangeCount) {
    if paths.is_empty() {
        return;
    }
    let urls: Vec<Retained<NSURL>> = paths
        .iter()
        .filter_map(|p| {
            let s = p.to_str()?;
            let ns = NSString::from_str(s);
            unsafe { NSURL::fileURLWithPath(&ns) }.into()
        })
        .collect();
    if urls.is_empty() {
        return;
    }
    // NSPasteboard.writeObjects takes NSArray<NSPasteboardWriting>; NSURL
    // conforms to NSPasteboardWriting (under the `NSPasteboard` feature
    // we already enable).
    let writers: Vec<Retained<ProtocolObject<dyn objc2_app_kit::NSPasteboardWriting>>> = urls
        .into_iter()
        .map(ProtocolObject::from_retained)
        .collect();
    let array = NSArray::from_vec(writers);
    let new_change_count = unsafe {
        let pb = NSPasteboard::generalPasteboard();
        pb.clearContents();
        pb.writeObjects(&array);
        pb.changeCount() as i64
    };
    // Tell the Mac-side poller "we just wrote these; ignore the next
    // changeCount tick."
    self_change_count.store(new_change_count, Ordering::Relaxed);
}

/// Download a single file as a fan-out of RANGE chunks. Up to
/// `MAX_PARALLEL_CHUNKS` requests are in flight at once; each completes
/// independently and writes its slice into the pre-allocated destination
/// at the matching offset. Returns `Ok(())` once every chunk has been
/// written.
///
/// Each chunk task opens its own `File` handle and `seek`s to its
/// position before writing. On Unix that's race-free: each open fd has
/// its own offset, and `pwrite`-like behavior via `seek+write` works as
/// long as no two tasks write the same byte range (which by construction
/// they don't — chunks are disjoint by `position`).
async fn fetch_file(
    file: &RemoteFile,
    dst: &Path,
    router: &DownloadRouter,
    sender: &EventSender,
) -> Result<(), String> {
    let total = match file.size {
        Some(s) => s,
        None => fetch_size(file.index, router, sender).await?,
    };

    // Pre-allocate the file to its final size so concurrent writers don't
    // race on extending it. `set_len` writes a sparse hole on APFS; the
    // actual blocks materialize as chunks land.
    {
        let f = std::fs::File::create(dst).map_err(|e| format!("create {dst:?}: {e}"))?;
        f.set_len(total)
            .map_err(|e| format!("set_len {dst:?}: {e}"))?;
    }

    if total == 0 {
        return Ok(());
    }

    // Build the chunk plan upfront so we know how many tasks to spawn.
    let mut plan: Vec<(u64, u32)> = Vec::new();
    let mut pos = 0u64;
    while pos < total {
        let req = (total - pos).min(u64::from(CHUNK_SIZE)) as u32;
        plan.push((pos, req));
        pos += u64::from(req);
    }

    let mut set: tokio::task::JoinSet<Result<(), String>> = tokio::task::JoinSet::new();
    let mut next = 0usize;
    let dst_owned = dst.to_path_buf();
    let index = file.index;

    // Prime the in-flight window.
    while next < plan.len() && set.len() < MAX_PARALLEL_CHUNKS {
        let (p, sz) = plan[next];
        set.spawn(chunk_task(
            index,
            p,
            sz,
            router.clone(),
            sender.clone(),
            dst_owned.clone(),
        ));
        next += 1;
    }

    // As each chunk completes, refill the window until the plan is
    // exhausted; collect any error and abort the rest.
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                set.abort_all();
                return Err(e);
            }
            Err(join_err) => {
                set.abort_all();
                return Err(format!("chunk task panicked: {join_err}"));
            }
        }
        if next < plan.len() {
            let (p, sz) = plan[next];
            set.spawn(chunk_task(
                index,
                p,
                sz,
                router.clone(),
                sender.clone(),
                dst_owned.clone(),
            ));
            next += 1;
        }
    }
    Ok(())
}

async fn chunk_task(
    index: i32,
    position: u64,
    requested_size: u32,
    router: DownloadRouter,
    sender: EventSender,
    dst: PathBuf,
) -> Result<(), String> {
    let bytes = fetch_range(index, position, requested_size, &router, &sender).await?;
    if bytes.is_empty() {
        return Err(format!(
            "short read at {position}; expected {requested_size} bytes, got 0"
        ));
    }
    // std::fs is fine here; writes are small, infrequent per task, and
    // tokio's blocking-thread pool absorbs the cost. Wrapping in
    // spawn_blocking would add more overhead than the save.
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .open(&dst)
        .map_err(|e| format!("open {dst:?}: {e}"))?;
    f.seek(SeekFrom::Start(position))
        .map_err(|e| format!("seek {dst:?} @ {position}: {e}"))?;
    f.write_all(&bytes)
        .map_err(|e| format!("write {dst:?} @ {position}: {e}"))?;
    Ok(())
}

async fn fetch_size(
    index: i32,
    router: &DownloadRouter,
    sender: &EventSender,
) -> Result<u64, String> {
    let (stream_id, rx) = router.register();
    push_request(
        sender,
        FileContentsRequest {
            stream_id,
            index,
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
    index: i32,
    position: u64,
    requested_size: u32,
    router: &DownloadRouter,
    sender: &EventSender,
) -> Result<Vec<u8>, String> {
    let (stream_id, rx) = router.register();
    push_request(
        sender,
        FileContentsRequest {
            stream_id,
            index,
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
