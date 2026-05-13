use std::net::SocketAddr;
use std::num::{NonZeroU16, NonZeroUsize};
use std::sync::Arc;

use anyhow::{Context, Result};
use bytes::Bytes;
use clap::Parser;
use ironrdp_server::{
    BitmapUpdate, Credentials, DesktopSize, DisplayUpdate, PixelFormat, RdpServer,
    RdpServerDisplay, RdpServerDisplayUpdates,
};
use rcgen::{generate_simple_self_signed, CertifiedKey};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;
use tracing::info;

#[derive(Parser, Debug)]
#[command(name = "macrdp", about = "Native RDP server for macOS")]
struct Args {
    /// Address to bind. Default is unprivileged 3390; real RDP is 3389.
    #[arg(long, default_value = "0.0.0.0:3390")]
    bind: SocketAddr,

    #[arg(long, default_value_t = 1280)]
    width: u16,

    #[arg(long, default_value_t = 720)]
    height: u16,

    /// Username the client must present. Plain-TLS RDP compares creds against
    /// what's set here; NLA/CredSSP would validate them differently.
    #[arg(long, default_value = "user")]
    username: String,

    /// Password the client must present. Dev-only — do not use a real password
    /// here; the demo is not hardened.
    #[arg(long, default_value = "password")]
    password: String,
}

struct StaticDisplay {
    width: u16,
    height: u16,
}

struct StaticUpdates {
    pending: Option<DisplayUpdate>,
}

#[async_trait::async_trait]
impl RdpServerDisplayUpdates for StaticUpdates {
    async fn next_update(&mut self) -> Result<Option<DisplayUpdate>> {
        if let Some(update) = self.pending.take() {
            return Ok(Some(update));
        }
        // After the first frame, park forever so the server keeps the session open.
        // Replacing this with a real frame source is the next step.
        std::future::pending::<()>().await;
        Ok(None)
    }
}

#[async_trait::async_trait]
impl RdpServerDisplay for StaticDisplay {
    async fn size(&mut self) -> DesktopSize {
        DesktopSize {
            width: self.width,
            height: self.height,
        }
    }

    async fn updates(&mut self) -> Result<Box<dyn RdpServerDisplayUpdates>> {
        let width = NonZeroU16::new(self.width).context("width must be > 0")?;
        let height = NonZeroU16::new(self.height).context("height must be > 0")?;
        let stride =
            NonZeroUsize::new(usize::from(self.width) * 4).context("stride must be > 0")?;
        let pixel_count = usize::from(self.width) * usize::from(self.height);

        // ARgb32 = bytes are A, R, G, B per pixel. Solid muted teal.
        let mut data = Vec::with_capacity(pixel_count * 4);
        for _ in 0..pixel_count {
            data.extend_from_slice(&[0xFF, 0x10, 0x80, 0x90]);
        }

        Ok(Box::new(StaticUpdates {
            pending: Some(DisplayUpdate::Bitmap(BitmapUpdate {
                x: 0,
                y: 0,
                width,
                height,
                format: PixelFormat::ARgb32,
                data: Bytes::from(data),
                stride,
            })),
        }))
    }
}

fn make_tls_acceptor() -> Result<TlsAcceptor> {
    let CertifiedKey { cert, key_pair } =
        generate_simple_self_signed(vec!["localhost".to_string()])
            .context("generate self-signed cert")?;
    let cert_der = CertificateDer::from(cert.der().to_vec());
    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der()));
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .context("build rustls ServerConfig")?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new(
                    "info,macrdp=debug,ironrdp_server=debug,ironrdp_acceptor=debug",
                )
            }),
        )
        .init();

    let args = Args::parse();

    #[cfg(not(target_os = "macos"))]
    tracing::warn!("Built for a non-macOS target — capture and input layers are stubs.");

    let tls = make_tls_acceptor()?;
    let display = StaticDisplay {
        width: args.width,
        height: args.height,
    };

    let mut server = RdpServer::builder()
        .with_addr(args.bind)
        .with_tls(tls)
        .with_no_input()
        .with_display_handler(display)
        .build();

    server.set_credentials(Some(Credentials {
        username: args.username.clone(),
        password: args.password.clone(),
        domain: None,
    }));

    info!(
        addr = %args.bind,
        user = %args.username,
        "macrdp listening — try: xfreerdp /v:127.0.0.1:{} /cert:ignore /u:{} /p:{}",
        args.bind.port(),
        args.username,
        args.password,
    );
    server.run().await
}
