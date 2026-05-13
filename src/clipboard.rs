//! Bidirectional text clipboard sync between the Mac and the RDP client.
//!
//! Only `CF_UNICODETEXT` ↔ `NSPasteboardTypeString` is wired today; images,
//! RTF, and file lists are stubbed out. The factory owns the event sender
//! and spawns a poller that detects Mac-side clipboard changes via
//! `NSPasteboard.changeCount` and signals the protocol layer.

use std::sync::{Arc, Mutex};

use ironrdp_cliprdr::backend::{
    CliprdrBackend, CliprdrBackendFactory, ClipboardMessage,
};
use ironrdp_cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags, FileContentsRequest,
    FileContentsResponse, FormatDataRequest, FormatDataResponse, LockDataId,
    OwnedFormatDataResponse,
};
use ironrdp_server::{CliprdrServerFactory, ServerEvent, ServerEventSender};
use tokio::sync::mpsc;
use tracing::{debug, warn};

type Sender = Arc<Mutex<Option<mpsc::UnboundedSender<ServerEvent>>>>;

#[derive(Debug)]
pub struct MacCliprdr {
    sender: Sender,
}

impl MacCliprdr {
    pub fn new() -> Self {
        Self {
            sender: Arc::new(Mutex::new(None)),
        }
    }
}

impl ServerEventSender for MacCliprdr {
    fn set_sender(&mut self, sender: mpsc::UnboundedSender<ServerEvent>) {
        *self.sender.lock().unwrap() = Some(sender);

        // Spawn a poller that notices Mac-side copies and tells the RDP
        // server to advertise the new content to the remote.
        let sender_arc = self.sender.clone();
        tokio::spawn(async move {
            // NSPasteboard.changeCount is monotonic; record the starting
            // value so we don't fire an event for whatever was already on
            // the clipboard when macrdp launched.
            let mut last_seen = pb::change_count();
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                let current = pb::change_count();
                if current == last_seen {
                    continue;
                }
                last_seen = current;
                if !pb::has_string() {
                    continue;
                }
                let formats =
                    vec![ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)];
                let guard = sender_arc.lock().unwrap();
                let Some(s) = guard.as_ref() else {
                    break; // sender dropped, server is shutting down
                };
                if s
                    .send(ServerEvent::Clipboard(ClipboardMessage::SendInitiateCopy(
                        formats,
                    )))
                    .is_err()
                {
                    break;
                }
            }
        });
    }
}

impl CliprdrBackendFactory for MacCliprdr {
    fn build_cliprdr_backend(&self) -> Box<dyn CliprdrBackend> {
        Box::new(MacCliprdrBackend {
            sender: self.sender.clone(),
        })
    }
}

impl CliprdrServerFactory for MacCliprdr {}

#[derive(Debug)]
struct MacCliprdrBackend {
    sender: Sender,
}

impl ironrdp_core::AsAny for MacCliprdrBackend {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
}

impl MacCliprdrBackend {
    fn push(&self, msg: ClipboardMessage) {
        if let Some(s) = self.sender.lock().unwrap().as_ref() {
            let _ = s.send(ServerEvent::Clipboard(msg));
        }
    }
}

impl CliprdrBackend for MacCliprdrBackend {
    fn temporary_directory(&self) -> &str {
        "/tmp"
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        ClipboardGeneralCapabilityFlags::empty()
    }

    fn on_ready(&mut self) {
        // Channel is up. If the Mac clipboard already has text, advertise it
        // so the remote can paste even without a fresh copy on the Mac side.
        if pb::has_string() {
            self.push(ClipboardMessage::SendInitiateCopy(vec![
                ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT),
            ]));
        }
    }

    fn on_request_format_list(&mut self) {
        // Same flow as on_ready — re-advertise the current Mac clipboard.
        if pb::has_string() {
            self.push(ClipboardMessage::SendInitiateCopy(vec![
                ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT),
            ]));
        }
    }

    fn on_process_negotiated_capabilities(
        &mut self,
        _capabilities: ClipboardGeneralCapabilityFlags,
    ) {
    }

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        // Remote (e.g. Windows) just put something on its clipboard. If they
        // offer Unicode text, ask for it; once we have the bytes we write
        // them to NSPasteboard.
        if let Some(fmt) = available_formats
            .iter()
            .find(|f| f.id == ClipboardFormatId::CF_UNICODETEXT)
        {
            self.push(ClipboardMessage::SendInitiatePaste(fmt.id));
        }
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        // Remote is asking for our Mac clipboard data in `request.format`.
        let response = if request.format == ClipboardFormatId::CF_UNICODETEXT {
            match pb::read_string() {
                Some(s) => {
                    // CF_UNICODETEXT is UTF-16 LE, null-terminated.
                    let mut units: Vec<u16> = s.encode_utf16().collect();
                    units.push(0);
                    let mut bytes = Vec::with_capacity(units.len() * 2);
                    for u in units {
                        bytes.extend_from_slice(&u.to_le_bytes());
                    }
                    OwnedFormatDataResponse::new_data(bytes)
                }
                None => OwnedFormatDataResponse::new_error(),
            }
        } else {
            OwnedFormatDataResponse::new_error()
        };
        self.push(ClipboardMessage::SendFormatData(response));
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        if response.is_error() {
            warn!("remote returned error for format data");
            return;
        }
        // CF_UNICODETEXT data is UTF-16 LE, optionally null-terminated.
        let data = response.data();
        if data.len() % 2 != 0 {
            warn!(len = data.len(), "odd-length UTF-16 payload");
            return;
        }
        let mut units: Vec<u16> = data
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        if matches!(units.last(), Some(0)) {
            units.pop();
        }
        match String::from_utf16(&units) {
            Ok(s) => {
                debug!(len = s.len(), "writing remote clipboard text to NSPasteboard");
                pb::write_string(&s);
            }
            Err(e) => warn!("UTF-16 decode failed: {e}"),
        }
    }

    fn on_file_contents_request(&mut self, _request: FileContentsRequest) {}
    fn on_file_contents_response(&mut self, _response: FileContentsResponse<'_>) {}
    fn on_lock(&mut self, _data_id: LockDataId) {}
    fn on_unlock(&mut self, _data_id: LockDataId) {}
}

#[cfg(target_os = "macos")]
mod pb {
    use objc2::rc::autoreleasepool;
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
    use objc2_foundation::NSString;

    pub fn change_count() -> i64 {
        unsafe {
            let pb = NSPasteboard::generalPasteboard();
            pb.changeCount() as i64
        }
    }

    pub fn has_string() -> bool {
        unsafe {
            let pb = NSPasteboard::generalPasteboard();
            let types = pb.types();
            let Some(types) = types else { return false };
            for i in 0..types.count() {
                let t = types.objectAtIndex(i);
                if (&*t).isEqualToString(NSPasteboardTypeString) {
                    return true;
                }
            }
            false
        }
    }

    pub fn read_string() -> Option<String> {
        autoreleasepool(|_| unsafe {
            let pb = NSPasteboard::generalPasteboard();
            pb.stringForType(NSPasteboardTypeString).map(|s| s.to_string())
        })
    }

    pub fn write_string(s: &str) {
        unsafe {
            let pb = NSPasteboard::generalPasteboard();
            pb.clearContents();
            let ns = NSString::from_str(s);
            pb.setString_forType(&ns, NSPasteboardTypeString);
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod pb {
    pub fn change_count() -> i64 {
        0
    }
    pub fn has_string() -> bool {
        false
    }
    pub fn read_string() -> Option<String> {
        None
    }
    pub fn write_string(_: &str) {}
}
