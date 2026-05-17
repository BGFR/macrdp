//! Bidirectional clipboard sync between the Mac and the RDP client.
//!
//! Text (CF_UNICODETEXT ↔ NSPasteboardTypeString) and images
//! (CF_DIB ↔ PNG/TIFF) flow both directions. File copy is **Mac → Windows
//! only and metadata only** today: the file *names* and sizes are advertised
//! as a FileGroupDescriptorW; any FileContentsRequest from the client is
//! answered with CB_RESPONSE_FAIL so the paste shows the list but no bytes
//! transfer. Streaming actual file bytes is Phase 2.
//!
//! The factory owns the event sender and spawns a poller that detects
//! Mac-side clipboard changes via `NSPasteboard.changeCount` and signals
//! the protocol layer.

use std::sync::{Arc, Mutex};

use std::io::Cursor;

use image::{ImageEncoder, ImageReader};
use ironrdp_cliprdr::backend::{ClipboardMessage, CliprdrBackend, CliprdrBackendFactory};
use ironrdp_cliprdr::pdu::{
    ClipboardFileAttributes, ClipboardFormat, ClipboardFormatId, ClipboardFormatName,
    ClipboardGeneralCapabilityFlags, FileContentsRequest, FileContentsResponse, FileDescriptor,
    FormatDataRequest, FormatDataResponse, LockDataId, OwnedFormatDataResponse, PackedFileList,
};
use ironrdp_core::{Encode, WriteCursor};
use ironrdp_server::{CliprdrServerFactory, ServerEvent, ServerEventSender};
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// Format ID we use locally to identify FileGroupDescriptorW in the format
/// list we advertise. The Windows side identifies the format by *name*, not
/// id, so this value is arbitrary — it just has to be in the
/// `0xC000..=0xFFFF` registered-format range. `0xC0FE` matches what upstream
/// IronRDP's `initiate_file_copy` uses, making the wire trace consistent
/// across implementations.
const FILE_LIST_FORMAT_ID: u32 = 0xC0FE;

type Sender = Arc<Mutex<Option<mpsc::UnboundedSender<ServerEvent>>>>;

/// Maximum FormatDataResponse payload we'll accept from the client. An
/// authenticated peer that paste-pumped a multi-gig DIB at us could
/// otherwise exhaust memory before any other check kicks in.
const MAX_INCOMING_PAYLOAD: usize = 50 * 1024 * 1024;

/// Convert PNG/TIFF bytes from NSPasteboard into a CF_DIB payload: a
/// `BITMAPINFOHEADER` (40 bytes) followed by 32bpp BGRA pixels in
/// top-down order (negative `biHeight`). 32bpp is the most widely
/// supported variant; we deliberately do not output BITMAPV5HEADER
/// since it complicates color-space negotiation with older clients.
fn png_or_tiff_to_dib(bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    let img = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()?
        .decode()?
        .to_rgba8();
    let (w, h) = (img.width(), img.height());
    let row_bytes = (w as usize) * 4;
    let pixel_bytes = row_bytes * (h as usize);

    let mut out = Vec::with_capacity(40 + pixel_bytes);
    // BITMAPINFOHEADER
    out.extend_from_slice(&40u32.to_le_bytes()); // biSize
    out.extend_from_slice(&(w as i32).to_le_bytes()); // biWidth
    out.extend_from_slice(&(-(h as i32)).to_le_bytes()); // biHeight (negative = top-down)
    out.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    out.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    out.extend_from_slice(&0u32.to_le_bytes()); // biCompression = BI_RGB
    out.extend_from_slice(&(pixel_bytes as u32).to_le_bytes()); // biSizeImage
    out.extend_from_slice(&0u32.to_le_bytes()); // biXPelsPerMeter
    out.extend_from_slice(&0u32.to_le_bytes()); // biYPelsPerMeter
    out.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
    out.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant

    // RGBA → BGRA, row order already top-down.
    for px in img.pixels() {
        let [r, g, b, a] = px.0;
        out.extend_from_slice(&[b, g, r, a]);
    }
    Ok(out)
}

/// Parse a CF_DIB / CF_DIBV5 payload into PNG bytes. We accept any
/// header size ≥ 40 (BITMAPINFOHEADER), 24bpp or 32bpp uncompressed
/// pixels (BI_RGB), top-down or bottom-up. Anything else is rejected
/// with an error.
fn dib_to_png(dib: &[u8]) -> anyhow::Result<Vec<u8>> {
    use anyhow::{anyhow, bail};
    if dib.len() < 40 {
        bail!("DIB shorter than BITMAPINFOHEADER");
    }
    let bi_size = u32::from_le_bytes(dib[0..4].try_into().unwrap()) as usize;
    if bi_size < 40 || bi_size > dib.len() {
        bail!("bogus biSize {bi_size}");
    }
    let width = i32::from_le_bytes(dib[4..8].try_into().unwrap());
    let height_signed = i32::from_le_bytes(dib[8..12].try_into().unwrap());
    let bit_count = u16::from_le_bytes(dib[14..16].try_into().unwrap());
    let compression = u32::from_le_bytes(dib[16..20].try_into().unwrap());

    if width <= 0 {
        bail!("invalid width {width}");
    }
    if height_signed == 0 {
        bail!("invalid height 0");
    }
    // BI_RGB (0) we treat as canonical layout. BI_BITFIELDS (3) we accept
    // for 32bpp under the assumption of standard ARGB masks
    //   (R=0x00FF0000, G=0x0000FF00, B=0x000000FF, A=0xFF000000)
    // — which is the only layout modern Windows actually emits. The masks
    // are stored differently per header version:
    //   BITMAPINFOHEADER (40):       12 bytes of RGB masks AFTER the header
    //   BITMAPV4HEADER  (108):       masks are INSIDE the header
    //   BITMAPV5HEADER  (124):       masks are INSIDE the header
    let bitfields = compression == 3 || compression == 6; // BI_BITFIELDS / BI_ALPHABITFIELDS
    if compression != 0 && !bitfields {
        bail!("unsupported BI_COMPRESSION {compression}");
    }
    if bit_count != 24 && bit_count != 32 {
        bail!("unsupported biBitCount {bit_count}");
    }
    if bitfields && bit_count != 32 {
        bail!("BI_BITFIELDS with biBitCount={bit_count} not supported");
    }

    let w = width as u32;
    let h = height_signed.unsigned_abs();
    let top_down = height_signed < 0;
    let bpp = (bit_count / 8) as usize;
    // BMP rows are padded to a 4-byte multiple.
    let stride = (w as usize * bpp + 3) & !3;
    // Pixel data starts after the header AND any out-of-band masks
    // (BITMAPINFOHEADER + BI_BITFIELDS = masks follow header).
    let mask_bytes = if bitfields && bi_size == 40 {
        if compression == 6 {
            16 // RGBA masks
        } else {
            12 // RGB masks
        }
    } else {
        0
    };
    let pixel_start = bi_size + mask_bytes;
    let need = pixel_start
        .checked_add(
            stride
                .checked_mul(h as usize)
                .ok_or_else(|| anyhow!("overflow"))?,
        )
        .ok_or_else(|| anyhow!("overflow"))?;
    if dib.len() < need {
        bail!("DIB payload truncated: have {}, need {need}", dib.len());
    }

    // Capacity arithmetic must match the byte-bounds checked_mul above —
    // otherwise an attacker could craft a DIB whose dimensions overflow u32
    // and silently allocate a too-small buffer. Vec would still grow on
    // push, so no UB, but be consistent.
    let cap = (w as usize)
        .checked_mul(h as usize)
        .and_then(|n| n.checked_mul(4))
        .ok_or_else(|| anyhow!("RGBA buffer size overflow"))?;
    let mut rgba: Vec<u8> = Vec::with_capacity(cap);
    for row in 0..h {
        let src_row = if top_down { row } else { h - 1 - row };
        let row_off = pixel_start + (src_row as usize) * stride;
        let row_bytes = &dib[row_off..row_off + w as usize * bpp];
        for chunk in row_bytes.chunks_exact(bpp) {
            // BMP pixels are BGR(A); convert to RGBA.
            let (b, g, r, a) = if bpp == 4 {
                (chunk[0], chunk[1], chunk[2], chunk[3])
            } else {
                (chunk[0], chunk[1], chunk[2], 0xFF)
            };
            rgba.extend_from_slice(&[r, g, b, a]);
        }
    }

    let mut png = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut png);
    encoder.write_image(&rgba, w, h, image::ExtendedColorType::Rgba8)?;
    Ok(png)
}

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
                let formats = advertised_formats();
                if formats.is_empty() {
                    continue;
                }
                let guard = sender_arc.lock().unwrap();
                let Some(s) = guard.as_ref() else {
                    break; // sender dropped, server is shutting down
                };
                if s.send(ServerEvent::Clipboard(ClipboardMessage::SendInitiateCopy(
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
            last_requested: None,
        })
    }
}

impl CliprdrServerFactory for MacCliprdr {}

#[derive(Debug)]
struct MacCliprdrBackend {
    sender: Sender,
    // Format we last asked the remote to send us. on_format_data_response
    // doesn't include the format ID, so we keep it here to know whether to
    // decode the payload as UTF-16 text or as a DIB.
    last_requested: Option<ClipboardFormatId>,
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

/// Build the format list to advertise based on what's currently on the
/// Mac pasteboard. Order is best-format-first; clients pick what they
/// can handle.
fn advertised_formats() -> Vec<ClipboardFormat> {
    let mut out = Vec::new();
    // Files are checked before image/text because in macOS multi-selection
    // a Finder copy can briefly look like "string + url" before the file URLs
    // settle, and we want the file flavor to win when present.
    if pb::has_files() {
        out.push(
            ClipboardFormat::new(ClipboardFormatId::new(FILE_LIST_FORMAT_ID))
                .with_name(ClipboardFormatName::FILE_LIST),
        );
    }
    if pb::has_image() {
        out.push(ClipboardFormat::new(ClipboardFormatId::CF_DIB));
    }
    if pb::has_string() {
        out.push(ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT));
    }
    out
}

/// Serialize the current Mac file-pasteboard contents as a
/// FILEGROUPDESCRIPTORW (`PackedFileList`) wire payload. Returns None if
/// nothing on the pasteboard maps to a regular file/dir we can describe.
fn encode_file_list() -> Option<Vec<u8>> {
    let entries = pb::read_files();
    if entries.is_empty() {
        return None;
    }
    let files: Vec<FileDescriptor> = entries
        .into_iter()
        .map(|e| {
            let mut fd = FileDescriptor::new(e.name);
            if e.is_dir {
                fd = fd.with_attributes(ClipboardFileAttributes::DIRECTORY);
            } else {
                fd = fd.with_attributes(ClipboardFileAttributes::NORMAL);
                if let Some(sz) = e.size {
                    fd = fd.with_file_size(sz);
                }
            }
            fd
        })
        .collect();
    let pdu = PackedFileList { files };
    let mut buf = vec![0u8; pdu.size()];
    let mut cur = WriteCursor::new(&mut buf);
    if let Err(e) = pdu.encode(&mut cur) {
        warn!("PackedFileList encode failed: {e}");
        return None;
    }
    Some(buf)
}

impl CliprdrBackend for MacCliprdrBackend {
    fn temporary_directory(&self) -> &str {
        "/tmp"
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        // STREAM_FILECLIP_ENABLED is the gate that lets either side use
        // FileGroupDescriptorW + FileContents{Request,Response}. Without
        // it, clients won't advertise file paste at all.
        ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED
    }

    fn on_ready(&mut self) {
        let formats = advertised_formats();
        if !formats.is_empty() {
            self.push(ClipboardMessage::SendInitiateCopy(formats));
        }
    }

    fn on_request_format_list(&mut self) {
        let formats = advertised_formats();
        if !formats.is_empty() {
            self.push(ClipboardMessage::SendInitiateCopy(formats));
        }
    }

    fn on_process_negotiated_capabilities(
        &mut self,
        _capabilities: ClipboardGeneralCapabilityFlags,
    ) {
    }

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        // Remote (e.g. Windows) put something on its clipboard. Prefer
        // DIBV5 over DIB (better color), then text. Asking for one format
        // doesn't preclude later asking for another; we only need the user's
        // single paste action so the first match wins.
        let priority = [
            ClipboardFormatId::CF_DIBV5,
            ClipboardFormatId::CF_DIB,
            ClipboardFormatId::CF_UNICODETEXT,
        ];
        for pref in priority {
            if let Some(fmt) = available_formats.iter().find(|f| f.id == pref) {
                self.last_requested = Some(fmt.id);
                self.push(ClipboardMessage::SendInitiatePaste(fmt.id));
                return;
            }
        }
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        // FileGroupDescriptorW is identified by id (the one we advertised),
        // not by a CF_* constant.
        if request.format == ClipboardFormatId::new(FILE_LIST_FORMAT_ID) {
            let response = match encode_file_list() {
                Some(bytes) => OwnedFormatDataResponse::new_data(bytes),
                None => OwnedFormatDataResponse::new_error(),
            };
            self.push(ClipboardMessage::SendFormatData(response));
            return;
        }
        let response = match request.format {
            ClipboardFormatId::CF_UNICODETEXT => match pb::read_string() {
                Some(s) => {
                    let mut units: Vec<u16> = s.encode_utf16().collect();
                    units.push(0);
                    let mut bytes = Vec::with_capacity(units.len() * 2);
                    for u in units {
                        bytes.extend_from_slice(&u.to_le_bytes());
                    }
                    OwnedFormatDataResponse::new_data(bytes)
                }
                None => OwnedFormatDataResponse::new_error(),
            },
            ClipboardFormatId::CF_DIB => match pb::read_image_bytes() {
                Some((_enc, bytes)) => match png_or_tiff_to_dib(&bytes) {
                    Ok(dib) => OwnedFormatDataResponse::new_data(dib),
                    Err(e) => {
                        warn!("DIB encode failed: {e}");
                        OwnedFormatDataResponse::new_error()
                    }
                },
                None => OwnedFormatDataResponse::new_error(),
            },
            other => {
                debug!(?other, "unsupported format requested by remote");
                OwnedFormatDataResponse::new_error()
            }
        };
        self.push(ClipboardMessage::SendFormatData(response));
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        let requested = self.last_requested.take();
        if response.is_error() {
            warn!("remote returned error for format data");
            return;
        }
        let data = response.data();
        if data.len() > MAX_INCOMING_PAYLOAD {
            warn!(
                len = data.len(),
                cap = MAX_INCOMING_PAYLOAD,
                "clipboard payload exceeds cap; dropping"
            );
            return;
        }
        match requested {
            Some(ClipboardFormatId::CF_UNICODETEXT) | None => {
                // Default to text if we don't know what we asked for —
                // matches the previous text-only behaviour.
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
                        debug!(
                            len = s.len(),
                            "writing remote clipboard text to NSPasteboard"
                        );
                        pb::write_string(&s);
                    }
                    Err(e) => warn!("UTF-16 decode failed: {e}"),
                }
            }
            Some(ClipboardFormatId::CF_DIB) | Some(ClipboardFormatId::CF_DIBV5) => {
                match dib_to_png(data) {
                    Ok(png) => {
                        debug!(
                            len = png.len(),
                            "writing remote clipboard image to NSPasteboard"
                        );
                        pb::write_png(&png);
                    }
                    Err(e) => warn!("DIB decode failed: {e}"),
                }
            }
            Some(other) => {
                warn!(?other, "unexpected format in data response");
            }
        }
    }

    fn on_file_contents_request(&mut self, request: FileContentsRequest) {
        // Phase 1: we advertise the file list (names + sizes) but don't
        // stream contents. Reply with CB_RESPONSE_FAIL so the client knows
        // the paste failed instead of hanging on a missing response.
        debug!(
            stream_id = request.stream_id,
            index = request.index,
            "file contents requested; replying with error (Phase 1 metadata-only)"
        );
        self.push(ClipboardMessage::SendFileContentsResponse(
            FileContentsResponse::new_error(request.stream_id),
        ));
    }
    fn on_file_contents_response(&mut self, _response: FileContentsResponse<'_>) {}
    fn on_lock(&mut self, _data_id: LockDataId) {}
    fn on_unlock(&mut self, _data_id: LockDataId) {}
}

#[cfg(target_os = "macos")]
mod pb {
    use objc2::rc::autoreleasepool;
    use objc2_app_kit::{
        NSPasteboard, NSPasteboardTypeFileURL, NSPasteboardTypePNG, NSPasteboardTypeString,
        NSPasteboardTypeTIFF,
    };
    use objc2_foundation::{NSData, NSString};

    pub fn change_count() -> i64 {
        unsafe {
            let pb = NSPasteboard::generalPasteboard();
            pb.changeCount() as i64
        }
    }

    pub fn has_string() -> bool {
        unsafe { has_type(NSPasteboardTypeString) }
    }

    pub fn has_image() -> bool {
        unsafe { has_type(NSPasteboardTypePNG) || has_type(NSPasteboardTypeTIFF) }
    }

    pub fn has_files() -> bool {
        unsafe { has_type(NSPasteboardTypeFileURL) }
    }

    pub struct FileEntry {
        pub name: String,
        pub size: Option<u64>,
        pub is_dir: bool,
    }

    /// Return one entry per file URL item on the general pasteboard.
    /// Cocoa stores multi-file selections as one pasteboard item per file,
    /// each with its own `NSPasteboardTypeFileURL` string. We resolve names
    /// and sizes via `std::fs::metadata`; unreadable paths are skipped so
    /// the rest of a selection still pastes.
    pub fn read_files() -> Vec<FileEntry> {
        autoreleasepool(|_| unsafe {
            let pb = NSPasteboard::generalPasteboard();
            let Some(items) = pb.pasteboardItems() else {
                return Vec::new();
            };
            let mut out = Vec::with_capacity(items.count());
            for i in 0..items.count() {
                let item = items.objectAtIndex(i);
                let Some(url_str) = item.stringForType(NSPasteboardTypeFileURL) else {
                    continue;
                };
                let url = url_str.to_string();
                let Some(path) = file_url_to_path(&url) else {
                    continue;
                };
                let Some(name) = std::path::Path::new(&path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(str::to_owned)
                else {
                    continue;
                };
                let meta = std::fs::metadata(&path).ok();
                let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
                // For directories, the wire `file_size` field is unused
                // (Windows queries it per-file when recursing).
                let size = if is_dir { None } else { meta.map(|m| m.len()) };
                out.push(FileEntry { name, size, is_dir });
            }
            out
        })
    }

    /// Convert a `file://` URL to a filesystem path. NSPasteboard hands us
    /// percent-encoded URLs (spaces -> `%20`, etc.) — strip the scheme and
    /// decode. Anything else (http://, raw paths) is rejected.
    fn file_url_to_path(url: &str) -> Option<String> {
        let rest = url.strip_prefix("file://")?;
        // RFC 3986 file URLs can have an empty authority (`file:///path`)
        // or a `localhost` authority (`file://localhost/path`). Strip both.
        let path = rest.strip_prefix("localhost").unwrap_or(rest);
        percent_decode(path)
    }

    fn percent_decode(s: &str) -> Option<String> {
        let bytes = s.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' && i + 2 < bytes.len() {
                let hi = (bytes[i + 1] as char).to_digit(16)?;
                let lo = (bytes[i + 2] as char).to_digit(16)?;
                out.push(((hi << 4) | lo) as u8);
                i += 3;
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        String::from_utf8(out).ok()
    }

    fn has_type(target: &objc2_app_kit::NSPasteboardType) -> bool {
        unsafe {
            let pb = NSPasteboard::generalPasteboard();
            let Some(types) = pb.types() else {
                return false;
            };
            for i in 0..types.count() {
                let t = types.objectAtIndex(i);
                if t.isEqualToString(target) {
                    return true;
                }
            }
            false
        }
    }

    pub fn read_string() -> Option<String> {
        autoreleasepool(|_| unsafe {
            let pb = NSPasteboard::generalPasteboard();
            pb.stringForType(NSPasteboardTypeString)
                .map(|s| s.to_string())
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

    /// Return the Mac clipboard's image, normalized to PNG bytes. Tries
    /// PNG first, falls back to TIFF (which we re-encode in clipboard.rs
    /// via the `image` crate so this returns PNG either way).
    pub fn read_image_bytes() -> Option<(ImageEncoding, Vec<u8>)> {
        autoreleasepool(|_| unsafe {
            let pb = NSPasteboard::generalPasteboard();
            if let Some(d) = pb.dataForType(NSPasteboardTypePNG) {
                return Some((ImageEncoding::Png, nsdata_to_vec(&d)));
            }
            if let Some(d) = pb.dataForType(NSPasteboardTypeTIFF) {
                return Some((ImageEncoding::Tiff, nsdata_to_vec(&d)));
            }
            None
        })
    }

    pub fn write_png(bytes: &[u8]) {
        unsafe {
            let pb = NSPasteboard::generalPasteboard();
            pb.clearContents();
            let data = NSData::with_bytes(bytes);
            pb.setData_forType(Some(&data), NSPasteboardTypePNG);
        }
    }

    fn nsdata_to_vec(d: &NSData) -> Vec<u8> {
        unsafe {
            let len = d.length();
            let ptr = d.bytes().as_ptr();
            std::slice::from_raw_parts(ptr, len).to_vec()
        }
    }

    pub enum ImageEncoding {
        Png,
        Tiff,
    }
}

#[cfg(not(target_os = "macos"))]
mod pb {
    pub enum ImageEncoding {
        Png,
        Tiff,
    }
    pub struct FileEntry {
        pub name: String,
        pub size: Option<u64>,
        pub is_dir: bool,
    }
    pub fn change_count() -> i64 {
        0
    }
    pub fn has_string() -> bool {
        false
    }
    pub fn has_image() -> bool {
        false
    }
    pub fn has_files() -> bool {
        false
    }
    pub fn read_string() -> Option<String> {
        None
    }
    pub fn write_string(_: &str) {}
    pub fn read_image_bytes() -> Option<(ImageEncoding, Vec<u8>)> {
        None
    }
    pub fn write_png(_: &[u8]) {}
    pub fn read_files() -> Vec<FileEntry> {
        Vec::new()
    }
}
