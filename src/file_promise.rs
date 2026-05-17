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

/// One entry in a remote-copy batch — file OR directory. Directories
/// don't have an `index` worth fetching but DO have a path we need to
/// `mkdir -p` before any descendant file lands.
#[derive(Clone, Debug)]
pub struct RemoteEntry {
    pub index: i32,
    pub name: String,
    pub size: Option<u64>,
    pub is_dir: bool,
    /// MS-RDPECLIP relative directory path (using `\` as the separator),
    /// e.g. `MyFolder\sub`. `None` for top-level entries.
    pub relative_path: Option<String>,
}

/// Spawned by `on_remote_file_list`: downloads every entry in `entries`
/// into a fresh temp directory (recreating the directory structure from
/// the `relative_path` fields), then publishes the **top-level** paths
/// (entries where `relative_path` is `None`) to NSPasteboard so Finder
/// pastes the folder tree rather than 1000 leaf URLs.
pub fn spawn_remote_paste(
    entries: Vec<RemoteEntry>,
    router: DownloadRouter,
    sender: EventSender,
    current_temp_dir: Arc<Mutex<Option<PathBuf>>>,
    self_change_count: SelfChangeCount,
    rt_handle: tokio::runtime::Handle,
) {
    if entries.is_empty() {
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

        // Compute every entry's full destination path up front. Path-
        // safety check rejects anything that escapes the temp root via
        // `..` or absolute components — a malicious remote could otherwise
        // craft a relative_path that writes outside the temp dir.
        let mut top_level: Vec<PathBuf> = Vec::new();
        let mut planned: Vec<(RemoteEntry, PathBuf)> = Vec::with_capacity(entries.len());
        for e in entries {
            let dst = match resolve_dest(&dir, &e) {
                Some(p) => p,
                None => {
                    warn!(name = %e.name, "rejecting entry with unsafe relative_path");
                    return;
                }
            };
            if e.relative_path.is_none() {
                top_level.push(dst.clone());
            }
            planned.push((e, dst));
        }

        // Create directories first so file fetches don't race the
        // mkdir. Sort directories shortest-path-first so parent dirs
        // exist before children.
        let mut dirs: Vec<&PathBuf> = planned
            .iter()
            .filter(|(e, _)| e.is_dir)
            .map(|(_, p)| p)
            .collect();
        dirs.sort_by_key(|p| p.components().count());
        for d in dirs {
            if let Err(err) = std::fs::create_dir_all(d) {
                warn!(?d, "create_dir_all failed: {err}");
                return;
            }
        }

        // Now fetch files. Sequential per-file (each file uses parallel
        // chunks internally), one failure aborts.
        for (e, dst) in &planned {
            if e.is_dir {
                continue;
            }
            // Some clients only put descendant files in the descriptor
            // list, not their parent directories. Ensure the dst's
            // parent exists in that case.
            if let Some(parent) = dst.parent() {
                if let Err(err) = std::fs::create_dir_all(parent) {
                    warn!(?parent, "create_dir_all for file parent failed: {err}");
                    return;
                }
            }
            match fetch_file_inner(e.index, e.size, dst, &router, &sender).await {
                Ok(()) => {
                    debug!(name = %e.name, dest = ?dst, "downloaded remote file");
                }
                Err(err) => {
                    warn!(name = %e.name, "download failed: {err}");
                    return;
                }
            }
        }

        publish_to_pasteboard(&top_level, &self_change_count);
        info!(
            files = planned.iter().filter(|(e, _)| !e.is_dir).count(),
            roots = top_level.len(),
            "published remote paste to NSPasteboard"
        );
        ready_signal();
    });
}

/// Compute the on-disk destination for a remote entry, joining its
/// `relative_path` components under `root` while rejecting any
/// path-traversal attempts. Returns `None` on unsafe input so the caller
/// can abort the whole paste.
fn resolve_dest(root: &Path, entry: &RemoteEntry) -> Option<PathBuf> {
    let mut p = root.to_path_buf();
    if let Some(rp) = &entry.relative_path {
        for component in rp.split('\\') {
            if component.is_empty() {
                continue;
            }
            if component == "." || component == ".." || component.contains('/') {
                return None;
            }
            p.push(component);
        }
    }
    if entry.name == "." || entry.name == ".." || entry.name.contains('/') {
        return None;
    }
    p.push(&entry.name);
    Some(p)
}

/// Audible "paste is ready" cue plus an opportunistic auto-Cmd-V if the
/// user is still in Finder.
///
/// We tried `osascript ... display notification` first but macOS attributes
/// the banner to the parent process (macrdp, unsigned, no notification
/// grant) and silently suppresses it. `afplay` doesn't go through the
/// notification permission system at all — it's just audio playback —
/// so the chime always plays. The Glass sound is short and unobtrusive.
///
/// The auto-paste piggy-backs on the same trigger because the typical
/// flow is "user copied in Windows, switched to Finder, hit Cmd-V, got
/// beep, is still staring at Finder." If Finder is the frontmost app
/// when our download finishes, fire `Cmd-V` via System Events so the
/// paste completes without the user having to do anything. If they've
/// switched focus to another app, the chime alone is the cue to switch
/// back and paste manually — we deliberately don't activate or steal
/// focus to avoid pasting into the wrong text field.
fn ready_signal() {
    play_glass();
    auto_paste_if_finder_front();
}

fn play_glass() {
    if let Err(e) = std::process::Command::new("afplay")
        .arg("/System/Library/Sounds/Glass.aiff")
        .spawn()
    {
        debug!("afplay spawn failed: {e}");
    }
}

fn auto_paste_if_finder_front() {
    // AppleScript: only fire Cmd-V if Finder is the currently frontmost
    // process. System Events handles the keystroke; the user only needs
    // to have granted Accessibility access to System Events once.
    let script = r#"
        tell application "System Events"
            set frontApp to name of first application process whose frontmost is true
            if frontApp is "Finder" then
                keystroke "v" using command down
            end if
        end tell
    "#;
    let output = match std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            debug!("osascript spawn failed for auto-paste: {e}");
            return;
        }
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        debug!(
            status = ?output.status,
            stderr = %stderr.trim(),
            "auto-paste osascript exited non-zero (typically: no Accessibility \
             grant for System Events, or Finder not running)"
        );
    } else {
        debug!("auto-paste check completed");
    }
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
async fn fetch_file_inner(
    index: i32,
    size_hint: Option<u64>,
    dst: &Path,
    router: &DownloadRouter,
    sender: &EventSender,
) -> Result<(), String> {
    let total = match size_hint {
        Some(s) => s,
        None => fetch_size(index, router, sender).await?,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, rp: Option<&str>) -> RemoteEntry {
        RemoteEntry {
            index: 0,
            name: name.to_owned(),
            size: None,
            is_dir: false,
            relative_path: rp.map(|s| s.to_owned()),
        }
    }

    #[test]
    fn resolve_dest_top_level_file() {
        let root = std::path::Path::new("/tmp/root");
        assert_eq!(
            resolve_dest(root, &entry("a.txt", None)).unwrap(),
            std::path::PathBuf::from("/tmp/root/a.txt")
        );
    }

    #[test]
    fn resolve_dest_joins_backslash_relative_path() {
        let root = std::path::Path::new("/tmp/root");
        assert_eq!(
            resolve_dest(root, &entry("foo.txt", Some("MyFolder\\sub"))).unwrap(),
            std::path::PathBuf::from("/tmp/root/MyFolder/sub/foo.txt")
        );
    }

    #[test]
    fn resolve_dest_rejects_dot_dot_in_relative_path() {
        let root = std::path::Path::new("/tmp/root");
        assert!(resolve_dest(root, &entry("p.txt", Some("..\\evil"))).is_none());
    }

    #[test]
    fn resolve_dest_rejects_dot_dot_in_name() {
        let root = std::path::Path::new("/tmp/root");
        assert!(resolve_dest(root, &entry("..", Some("MyFolder"))).is_none());
    }

    #[test]
    fn resolve_dest_rejects_forward_slash_in_component() {
        // A remote whose relative_path contains a literal `/` is trying
        // to smuggle an extra path component past our `\`-split. Reject.
        let root = std::path::Path::new("/tmp/root");
        assert!(resolve_dest(root, &entry("p.txt", Some("MyFolder/sub"))).is_none());
    }

    #[test]
    fn resolve_dest_handles_empty_relative_path() {
        let root = std::path::Path::new("/tmp/root");
        // Some clients send "" for top-level entries instead of None.
        assert_eq!(
            resolve_dest(root, &entry("a.txt", Some(""))).unwrap(),
            std::path::PathBuf::from("/tmp/root/a.txt")
        );
    }
}
