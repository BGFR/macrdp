//! Smart-card redirection bridge (macOS, `--enable-smartcard-redirection`).
//!
//! The connecting RDP client redirects its physical smart-card reader as an
//! RDPDR `Smartcard` device; this bridge makes that reader usable by **macOS**
//! apps. macrdp's own PC/SC IFD handler (the `ifd-macrdp.bundle` cdylib loaded by
//! `com.apple.ifdreader.slotd`) dials this bridge over loopback TCP and speaks a
//! tiny request/reply protocol; the bridge translates each request into an
//! MS-RDPESC call to the client's reader via [`RdpdrHandle`]'s `scard_*` methods
//! and hands the result back.
//!
//! Wire protocol (handler → bridge request / bridge → handler reply), `u16`
//! lengths big-endian — must stay in lockstep with `ifd-handler/src/lib.rs`:
//!   POWER_ON  (1)                → `[status:u8]`; if 0: `[atr_len:u8][atr…]`  (1 = no card)
//!   POWER_OFF (2)                → `[status:u8]`
//!   TRANSMIT  (3)[len:u16][apdu…] → `[status:u8]`; if 0: `[len:u16][resp…]`
//!   PRESENCE  (4)                → `[present:u8]`  (1 = card present)
//!
//! One [`SmartcardBridge`] per connection (the device id comes from the announced
//! `Smartcard` device); dropping it aborts the listener and every in-flight
//! session, so a client disconnect tears the bridge down. Each accepted handler
//! connection gets its own [`Session`] holding the established PC/SC context, the
//! chosen reader, and the connected card handle.

use anyhow::{anyhow, Result};
use ironrdp_rdpdr::pdu::esc::{
    CardProtocol, CardStateFlags, ScardContext, ScardHandle as ScardCardHandle,
};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use ironrdp_server::{RdpdrHandle, SCARD_LEAVE_CARD, SCARD_SHARE_SHARED};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpSocket, TcpStream};
use tokio::task::{JoinHandle, JoinSet};
use tracing::{debug, info, warn};

/// Default loopback port the IFD handler dials (overridable, in lockstep with the
/// handler, via `MACRDP_SCARD_PORT`).
const DEFAULT_PORT: u16 = 40242;

// Protocol opcodes — must match `ifd-handler/src/lib.rs`.
const CMD_POWER_ON: u8 = 1;
const CMD_POWER_OFF: u8 = 2;
const CMD_TRANSMIT: u8 = 3;
const CMD_PRESENCE: u8 = 4;

fn port() -> u16 {
    std::env::var("MACRDP_SCARD_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_PORT)
}

/// A running smart-card bridge. Holds the listener task; dropping it aborts the
/// task (and, via the task's `JoinSet`, every in-flight session).
#[derive(Debug)]
pub struct SmartcardBridge {
    task: JoinHandle<()>,
}

impl SmartcardBridge {
    /// Bind the loopback listener and start serving the IFD handler. `device_id`
    /// is the announced `Smartcard` device the calls are routed to.
    pub fn start(handle: RdpdrHandle, device_id: u32) -> Self {
        let listen_port = port();
        let task = tokio::spawn(async move {
            if let Err(e) = serve(handle, device_id, listen_port).await {
                warn!(error = %e, "smart card: IFD bridge listener stopped");
            }
        });
        Self { task }
    }
}

impl Drop for SmartcardBridge {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(handle: RdpdrHandle, device_id: u32, listen_port: u16) -> Result<()> {
    // SO_REUSEADDR so a quick client reconnect can rebind the fixed port even if
    // the previous bridge's listener is still winding down (or in TIME_WAIT).
    let socket = TcpSocket::new_v4().map_err(|e| anyhow!("socket: {e}"))?;
    socket
        .set_reuseaddr(true)
        .map_err(|e| anyhow!("set_reuseaddr: {e}"))?;
    let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, listen_port));
    socket.bind(addr).map_err(|e| anyhow!("bind {addr}: {e}"))?;
    let listener = socket.listen(16).map_err(|e| anyhow!("listen: {e}"))?;
    info!(
        port = listen_port,
        device_id, "smart card: IFD bridge listening"
    );

    // Sessions live in a JoinSet so aborting this task (bridge Drop) cancels them.
    let mut sessions: JoinSet<()> = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted.map_err(|e| anyhow!("accept: {e}"))?;
                let handle = handle.clone();
                sessions.spawn(async move {
                    let mut session = Session::new(handle, device_id);
                    if let Err(e) = session.run(stream).await {
                        debug!(error = %e, "smart card: session ended with error");
                    }
                    session.teardown().await;
                });
            }
            // Reap finished sessions so the set doesn't grow unbounded.
            Some(_) = sessions.join_next(), if !sessions.is_empty() => {}
        }
    }
}

/// Per-handler-connection PC/SC state. The IFD handler keeps one connection and
/// serializes its calls, so a `Session` is single-threaded in practice.
struct Session {
    handle: RdpdrHandle,
    device_id: u32,
    context: Option<ScardContext>,
    reader: Option<String>,
    card: Option<(ScardCardHandle, CardProtocol)>,
}

impl Session {
    fn new(handle: RdpdrHandle, device_id: u32) -> Self {
        Self {
            handle,
            device_id,
            context: None,
            reader: None,
            card: None,
        }
    }

    async fn run(&mut self, mut stream: TcpStream) -> Result<()> {
        loop {
            // A read error here is the handler closing the connection (EOF) — a
            // clean end, not a failure.
            let cmd = match stream.read_u8().await {
                Ok(c) => c,
                Err(_) => return Ok(()),
            };
            match cmd {
                CMD_PRESENCE => {
                    let present = self.presence().await.unwrap_or(false);
                    stream.write_u8(u8::from(present)).await?;
                }
                CMD_POWER_ON => match self.power_on().await {
                    Ok(atr) => {
                        let len = u8::try_from(atr.len()).unwrap_or(0);
                        stream.write_u8(0).await?;
                        stream.write_u8(len).await?;
                        stream.write_all(&atr[..len as usize]).await?;
                    }
                    Err(e) => {
                        debug!(error = %e, "smart card: power_on failed (reporting no card)");
                        stream.write_u8(1).await?;
                    }
                },
                CMD_TRANSMIT => {
                    let len = stream.read_u16().await? as usize;
                    let mut apdu = vec![0u8; len];
                    stream.read_exact(&mut apdu).await?;
                    match self.transmit(&apdu).await {
                        Ok(resp) if resp.len() <= usize::from(u16::MAX) => {
                            stream.write_u8(0).await?;
                            stream.write_u16(resp.len() as u16).await?;
                            stream.write_all(&resp).await?;
                        }
                        Ok(_) => {
                            warn!("smart card: transmit response exceeds 64 KiB; dropping");
                            stream.write_u8(1).await?;
                        }
                        Err(e) => {
                            debug!(error = %e, "smart card: transmit failed");
                            stream.write_u8(1).await?;
                        }
                    }
                }
                CMD_POWER_OFF => {
                    let _ = self.power_off().await;
                    stream.write_u8(0).await?;
                }
                other => {
                    warn!(opcode = other, "smart card: unknown bridge opcode; closing");
                    return Ok(());
                }
            }
        }
    }

    /// Lazily establish a PC/SC context and pick the first redirected reader.
    async fn ensure_session(&mut self) -> Result<(ScardContext, String)> {
        if self.context.is_none() {
            self.context = Some(self.handle.scard_establish_context(self.device_id).await?);
        }
        let context = self.context.expect("context just set");
        if self.reader.is_none() {
            let readers = self
                .handle
                .scard_list_readers(self.device_id, context)
                .await?;
            let reader = readers
                .into_iter()
                .next()
                .ok_or_else(|| anyhow!("client redirected no smart-card readers"))?;
            info!(reader = %reader, "smart card: using redirected reader");
            self.reader = Some(reader);
        }
        Ok((context, self.reader.clone().expect("reader just set")))
    }

    async fn presence(&mut self) -> Result<bool> {
        let (context, reader) = self.ensure_session().await?;
        let states = self
            .handle
            .scard_get_status_change(self.device_id, context, std::slice::from_ref(&reader), 0)
            .await?;
        Ok(states
            .iter()
            .any(|s| s.event_state.contains(CardStateFlags::SCARD_STATE_PRESENT)))
    }

    async fn power_on(&mut self) -> Result<Vec<u8>> {
        let (context, reader) = self.ensure_session().await?;
        if self.card.is_none() {
            let (card, protocol) = self
                .handle
                .scard_connect(
                    self.device_id,
                    context,
                    &reader,
                    SCARD_SHARE_SHARED,
                    CardProtocol::SCARD_PROTOCOL_T0 | CardProtocol::SCARD_PROTOCOL_T1,
                )
                .await?;
            self.card = Some((card, protocol));
        }
        let (card, _) = self.card.clone().expect("card just connected");
        let status = self.handle.scard_status(self.device_id, card).await?;
        let atr_len = usize::try_from(status.atr_length)
            .unwrap_or(0)
            .min(status.atr.len());
        Ok(status.atr[..atr_len].to_vec())
    }

    async fn transmit(&mut self, apdu: &[u8]) -> Result<Vec<u8>> {
        let (card, protocol) = self
            .card
            .clone()
            .ok_or_else(|| anyhow!("no card connected"))?;
        self.handle
            .scard_transmit(self.device_id, card, protocol, apdu)
            .await
    }

    async fn power_off(&mut self) -> Result<()> {
        if let Some((card, _)) = self.card.take() {
            self.handle
                .scard_disconnect(self.device_id, card, SCARD_LEAVE_CARD)
                .await?;
        }
        Ok(())
    }

    /// Best-effort release of the card + context when the handler disconnects
    /// cleanly. (On a hard RDP disconnect the session task is aborted before this
    /// runs; the client's resource manager reaps the orphaned context when the
    /// channel closes.)
    async fn teardown(&mut self) {
        if let Some((card, _)) = self.card.take() {
            let _ = self
                .handle
                .scard_disconnect(self.device_id, card, SCARD_LEAVE_CARD)
                .await;
        }
        if let Some(context) = self.context.take() {
            let _ = self
                .handle
                .scard_release_context(self.device_id, context)
                .await;
        }
    }
}
