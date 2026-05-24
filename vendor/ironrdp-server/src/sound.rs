pub use ironrdp_rdpsnd::server::{RdpsndServerHandler, RdpsndServerMessage};

use tokio::sync::mpsc;

use crate::ServerEventSender;

/// One captured audio chunk en route to the client: PCM bytes (already
/// in the negotiated format — typically 16-bit stereo interleaved at
/// 44.1 kHz) plus the source-side timestamp in ms. Carried on a
/// dedicated bounded channel separate from the unified `ServerEvent`
/// stream so the server's audio dispatch can run independently of
/// inbound-PDU and outbound-event dispatch, avoiding the multi-second
/// audio starvation that happens when inbound cliprdr chunks monopolize
/// the per-connection `Mutex<Self>` (e.g., during a large
/// `--lazy-paste` Windows→Mac transfer).
pub type AudioWave = (Vec<u8>, u32);

pub trait SoundServerFactory: ServerEventSender {
    fn build_backend(&self) -> Box<dyn RdpsndServerHandler>;

    /// Hands the audio backend a dedicated bounded sender for `Wave`
    /// PDUs. Default impl is a no-op for backends that haven't been
    /// updated yet — those still funnel waves through the unified
    /// `ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(...))` channel,
    /// but they lose the audio-dispatch isolation. Update the backend
    /// to override this and emit Wave PDUs via `audio_sender` instead.
    ///
    /// Channel is bounded (capacity sized to ~1 s of audio at our
    /// 42.78 waves/s steady-state rate) so the capture loop naturally
    /// backpressures (skips at the SCK level) instead of letting an
    /// unbounded queue grow if the dispatch task ever stalls.
    fn set_audio_sender(&mut self, _audio_sender: mpsc::Sender<AudioWave>) {}
}
