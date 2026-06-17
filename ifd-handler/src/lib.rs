//! macrdp PC/SC **IFD handler** — the user-space reader driver macOS's
//! SmartCardServices (`com.apple.ifdreader.slotd`) loads. It implements the
//! public **IFDHandler v3.0** C ABI and forwards every card operation to the
//! macrdp process over a tiny loopback-TCP protocol; macrdp turns those into
//! MS-RDPESC calls to the *client's* real reader. Written from scratch so we
//! don't ship the GPL `vpcd` from vsmartcard.
//!
//! slotd loads this only when a USB device matching the bundle Info.plist's
//! `ifdVendorID`/`ifdProductID` is present (the macOS hotplug-trigger quirk).
//! It then calls the `IFDH*` functions below; we declare ourselves NOT
//! thread-safe (see `IFDHGetCapabilities`) so slotd serializes the calls, and a
//! single global `Mutex` guards the connection + cached ATR.
//!
//! Protocol (handler → macrdp request / macrdp → handler reply), all on
//! `127.0.0.1:MACRDP_SCARD_PORT`:
//!   POWER_ON  (1)            → [status:u8]; if 0: [atr_len:u8][atr…]  (1 = no card)
//!   POWER_OFF (2)            → [status:u8]
//!   TRANSMIT  (3)[len:u16][apdu…] → [status:u8]; if 0: [len:u16][resp…]
//!   PRESENCE  (4)            → [present:u8]   (1 = card present)
//! (u16 lengths are big-endian.) When macrdp isn't listening (no client / no
//! redirected reader) connects fail and we report "no card".

#![allow(non_snake_case)]

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::os::raw::c_char;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

// ---- IFDHandler v3.0 C types ----
type Dword = u32;
type PUchar = *mut u8;
type PDword = *mut u32;
type ResponseCode = i32;
type Lpstr = *mut c_char;

#[repr(C)]
pub struct ScardIoHeader {
    protocol: Dword,
    length: Dword,
}

// ---- IFDHandler v3.0 result + action codes (from PCSC ifdhandler.h) ----
const IFD_SUCCESS: ResponseCode = 0;
const IFD_ERROR_TAG: ResponseCode = 600;
const IFD_ERROR_POWER_ACTION: ResponseCode = 608;
const IFD_COMMUNICATION_ERROR: ResponseCode = 612;
const IFD_NOT_SUPPORTED: ResponseCode = 614;
const IFD_ICC_PRESENT: ResponseCode = 615;
const IFD_ICC_NOT_PRESENT: ResponseCode = 616;

const IFD_POWER_UP: Dword = 500;
const IFD_POWER_DOWN: Dword = 501;
const IFD_RESET: Dword = 502;

const TAG_IFD_ATR: Dword = 0x0303;
const TAG_IFD_SLOT_THREAD_SAFE: Dword = 0x0FAC;
const TAG_IFD_THREAD_SAFE: Dword = 0x0FAD;
const TAG_IFD_SLOTS_NUMBER: Dword = 0x0FAE;
const TAG_IFD_SIMULTANEOUS_ACCESS: Dword = 0x0FAF;

const MAX_ATR: usize = 33;
const CONNECT_TIMEOUT: Duration = Duration::from_millis(300);
const IO_TIMEOUT: Duration = Duration::from_millis(5000);

// ---- protocol opcodes ----
const CMD_POWER_ON: u8 = 1;
const CMD_POWER_OFF: u8 = 2;
const CMD_TRANSMIT: u8 = 3;
const CMD_PRESENCE: u8 = 4;

/// The loopback port macrdp listens on. Overridable via `MACRDP_SCARD_PORT`
/// (read once at load) so a non-default macrdp build still connects.
fn port() -> u16 {
    std::env::var("MACRDP_SCARD_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(40242)
}

#[derive(Default)]
struct State {
    stream: Option<TcpStream>,
    atr: Vec<u8>,
}

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(State::default()))
}

impl State {
    /// Ensure a live connection to macrdp, dialing on demand. Returns the stream
    /// or `None` if macrdp isn't listening (no client / no redirected reader).
    fn conn(&mut self) -> Option<&mut TcpStream> {
        if self.stream.is_none() {
            let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port());
            match TcpStream::connect_timeout(&addr.into(), CONNECT_TIMEOUT) {
                Ok(s) => {
                    let _ = s.set_read_timeout(Some(IO_TIMEOUT));
                    let _ = s.set_write_timeout(Some(IO_TIMEOUT));
                    let _ = s.set_nodelay(true);
                    self.stream = Some(s);
                }
                Err(_) => return None,
            }
        }
        self.stream.as_mut()
    }

    /// Drop the connection so the next op redials (after any I/O error).
    fn reset(&mut self) {
        self.stream = None;
        self.atr.clear();
    }
}

/// Run `f` with a live connection; on any I/O error, reset and yield `Err(())`.
fn with_conn<T>(
    s: &mut State,
    f: impl FnOnce(&mut TcpStream) -> std::io::Result<T>,
) -> Result<T, ()> {
    let Some(stream) = s.conn() else {
        return Err(());
    };
    match f(stream) {
        Ok(v) => Ok(v),
        Err(_) => {
            s.reset();
            Err(())
        }
    }
}

fn read_u8(s: &mut TcpStream) -> std::io::Result<u8> {
    let mut b = [0u8; 1];
    s.read_exact(&mut b)?;
    Ok(b[0])
}

fn read_u16(s: &mut TcpStream) -> std::io::Result<u16> {
    let mut b = [0u8; 2];
    s.read_exact(&mut b)?;
    Ok(u16::from_be_bytes(b))
}

fn read_vec(s: &mut TcpStream, len: usize) -> std::io::Result<Vec<u8>> {
    let mut v = vec![0u8; len];
    s.read_exact(&mut v)?;
    Ok(v)
}

// ---- IFDHandler v3.0 entry points ----

/// Open the reader (called when slotd matches the trigger device). We connect
/// lazily per op, so just clear stale state.
#[no_mangle]
pub extern "C" fn IFDHCreateChannelByName(_Lun: Dword, _DeviceName: Lpstr) -> ResponseCode {
    state().lock().unwrap().reset();
    IFD_SUCCESS
}

#[no_mangle]
pub extern "C" fn IFDHCreateChannel(_Lun: Dword, _Channel: Dword) -> ResponseCode {
    state().lock().unwrap().reset();
    IFD_SUCCESS
}

#[no_mangle]
pub extern "C" fn IFDHCloseChannel(_Lun: Dword) -> ResponseCode {
    let mut s = state().lock().unwrap();
    // Best-effort power-off, then drop the connection.
    let _ = with_conn(&mut s, |c| {
        c.write_all(&[CMD_POWER_OFF]).and_then(|_| read_u8(c))
    });
    s.reset();
    IFD_SUCCESS
}

/// Report fixed capabilities. The buffer at `Value` has capacity `*Length` on
/// input; we write the value and set `*Length` to its real size.
#[no_mangle]
pub extern "C" fn IFDHGetCapabilities(
    _Lun: Dword,
    Tag: Dword,
    Length: PDword,
    Value: PUchar,
) -> ResponseCode {
    if Length.is_null() {
        return IFD_COMMUNICATION_ERROR;
    }
    let cap = unsafe { *Length } as usize;
    let write_bytes = |bytes: &[u8]| -> bool {
        if Value.is_null() || cap < bytes.len() {
            return false;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), Value, bytes.len());
            *Length = bytes.len() as u32;
        }
        true
    };
    match Tag {
        TAG_IFD_SLOTS_NUMBER | TAG_IFD_SIMULTANEOUS_ACCESS => {
            if write_bytes(&[1u8]) {
                IFD_SUCCESS
            } else {
                IFD_ERROR_TAG
            }
        }
        TAG_IFD_THREAD_SAFE | TAG_IFD_SLOT_THREAD_SAFE => {
            if write_bytes(&[0u8]) {
                IFD_SUCCESS
            } else {
                IFD_ERROR_TAG
            }
        }
        TAG_IFD_ATR => {
            let atr = state().lock().unwrap().atr.clone();
            if write_bytes(&atr) {
                IFD_SUCCESS
            } else {
                IFD_ERROR_TAG
            }
        }
        _ => IFD_ERROR_TAG,
    }
}

#[no_mangle]
pub extern "C" fn IFDHSetCapabilities(
    _Lun: Dword,
    _Tag: Dword,
    _Length: Dword,
    _Value: PUchar,
) -> ResponseCode {
    // We hold no settable capabilities; accept as a no-op.
    IFD_SUCCESS
}

#[no_mangle]
pub extern "C" fn IFDHSetProtocolParameters(
    _Lun: Dword,
    _Protocol: Dword,
    _Flags: u8,
    _PTS1: u8,
    _PTS2: u8,
    _PTS3: u8,
) -> ResponseCode {
    // The client's real reader negotiates the protocol; accept whatever slotd asks.
    IFD_SUCCESS
}

/// Power up / reset → fetch the card's ATR; power down → release it.
#[no_mangle]
pub extern "C" fn IFDHPowerICC(
    _Lun: Dword,
    Action: Dword,
    Atr: PUchar,
    AtrLength: PDword,
) -> ResponseCode {
    let mut s = state().lock().unwrap();
    match Action {
        IFD_POWER_UP | IFD_RESET => {
            let result = with_conn(&mut s, |c| {
                c.write_all(&[CMD_POWER_ON])?;
                let status = read_u8(c)?;
                if status != 0 {
                    return Ok(None); // no card
                }
                let len = read_u8(c)? as usize;
                Ok(Some(read_vec(c, len)?))
            });
            match result {
                Ok(Some(atr)) if !atr.is_empty() && atr.len() <= MAX_ATR => {
                    s.atr = atr.clone();
                    if Atr.is_null() || AtrLength.is_null() {
                        return IFD_COMMUNICATION_ERROR;
                    }
                    // Per the IFDHandler v3.0 contract the `Atr` buffer is always
                    // MAX_ATR_SIZE (33) and `*AtrLength` is OUTPUT-only — macOS
                    // slotd passes it in as 0, so we must NOT treat it as an input
                    // capacity bound. We already guard atr.len() <= MAX_ATR above.
                    unsafe {
                        std::ptr::copy_nonoverlapping(atr.as_ptr(), Atr, atr.len());
                        *AtrLength = atr.len() as u32;
                    }
                    IFD_SUCCESS
                }
                _ => {
                    s.atr.clear();
                    if !AtrLength.is_null() {
                        unsafe { *AtrLength = 0 };
                    }
                    IFD_ERROR_POWER_ACTION
                }
            }
        }
        IFD_POWER_DOWN => {
            let _ = with_conn(&mut s, |c| {
                c.write_all(&[CMD_POWER_OFF]).and_then(|_| read_u8(c))
            });
            s.atr.clear();
            if !AtrLength.is_null() {
                unsafe { *AtrLength = 0 };
            }
            IFD_SUCCESS
        }
        _ => IFD_NOT_SUPPORTED,
    }
}

/// Forward one command APDU to the client's card and return its response.
#[no_mangle]
pub extern "C" fn IFDHTransmitToICC(
    _Lun: Dword,
    _SendPci: ScardIoHeader,
    TxBuffer: PUchar,
    TxLength: Dword,
    RxBuffer: PUchar,
    RxLength: PDword,
    RecvPci: *mut ScardIoHeader,
) -> ResponseCode {
    if RxLength.is_null() {
        return IFD_COMMUNICATION_ERROR;
    }
    let cap = unsafe { *RxLength } as usize;
    let apdu: Vec<u8> = if TxLength == 0 || TxBuffer.is_null() {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(TxBuffer, TxLength as usize).to_vec() }
    };

    let mut s = state().lock().unwrap();
    let result = with_conn(&mut s, |c| {
        let mut req = Vec::with_capacity(3 + apdu.len());
        req.push(CMD_TRANSMIT);
        req.extend_from_slice(&(apdu.len() as u16).to_be_bytes());
        req.extend_from_slice(&apdu);
        c.write_all(&req)?;
        let status = read_u8(c)?;
        if status != 0 {
            return Ok(None);
        }
        let len = read_u16(c)? as usize;
        Ok(Some(read_vec(c, len)?))
    });

    match result {
        Ok(Some(resp)) => {
            if RxBuffer.is_null() || cap < resp.len() {
                return IFD_COMMUNICATION_ERROR;
            }
            unsafe {
                std::ptr::copy_nonoverlapping(resp.as_ptr(), RxBuffer, resp.len());
                *RxLength = resp.len() as u32;
                if !RecvPci.is_null() {
                    (*RecvPci).protocol = _SendPci.protocol;
                }
            }
            IFD_SUCCESS
        }
        _ => IFD_COMMUNICATION_ERROR,
    }
}

#[no_mangle]
pub extern "C" fn IFDHControl(
    _Lun: Dword,
    _dwControlCode: Dword,
    _TxBuffer: PUchar,
    _TxLength: Dword,
    _RxBuffer: PUchar,
    _RxLength: Dword,
    pdwBytesReturned: PDword,
) -> ResponseCode {
    if !pdwBytesReturned.is_null() {
        unsafe { *pdwBytesReturned = 0 };
    }
    IFD_NOT_SUPPORTED
}

/// Is a card present? Polled frequently by slotd.
#[no_mangle]
pub extern "C" fn IFDHICCPresence(_Lun: Dword) -> ResponseCode {
    let mut s = state().lock().unwrap();
    let present = with_conn(&mut s, |c| {
        c.write_all(&[CMD_PRESENCE])?;
        read_u8(c)
    });
    match present {
        Ok(1) => IFD_ICC_PRESENT,
        Ok(_) => IFD_ICC_NOT_PRESENT,
        Err(()) => IFD_ICC_NOT_PRESENT, // macrdp not listening → no card
    }
}
