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
use std::io::Write;
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
/// fetching files from us in the Mac→Windows direction.
const CHUNK_SIZE: u32 = 1024 * 1024;

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
    });
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

/// SIZE-then-RANGE loop for one file. Writes `dst` and returns
/// `Ok(())` on full success.
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
    let mut out = std::fs::File::create(dst).map_err(|e| format!("create {dst:?}: {e}"))?;
    let mut position: u64 = 0;
    while position < total {
        let remaining = total - position;
        let req_size = remaining.min(u64::from(CHUNK_SIZE)) as u32;
        let chunk = fetch_range(file.index, position, req_size, router, sender).await?;
        if chunk.is_empty() {
            return Err(format!(
                "short read at {position}; expected {req_size} bytes, got 0"
            ));
        }
        out.write_all(&chunk)
            .map_err(|e| format!("write {dst:?}: {e}"))?;
        position += chunk.len() as u64;
    }
    out.flush().map_err(|e| format!("flush {dst:?}: {e}"))?;
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
