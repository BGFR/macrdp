use anyhow::Result;
use clap::Parser;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tracing::{info, warn};

#[derive(Parser, Debug)]
#[command(name = "macrdp", about = "Native RDP server for macOS")]
struct Args {
    /// Address to bind. Default is unprivileged 3390; real RDP is 3389.
    #[arg(long, default_value = "0.0.0.0:3390")]
    bind: SocketAddr,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,macrdp=debug")),
        )
        .init();

    let args = Args::parse();

    #[cfg(not(target_os = "macos"))]
    warn!("Built for a non-macOS target — capture and input layers are stubs.");

    let listener = TcpListener::bind(args.bind).await?;
    info!(addr = %args.bind, "macrdp listening");

    loop {
        let (stream, peer) = listener.accept().await?;
        info!(%peer, "accepted connection");
        tokio::spawn(async move {
            if let Err(err) = handle_session(stream).await {
                warn!(%peer, ?err, "session ended with error");
            }
        });
    }
}

async fn handle_session(_stream: tokio::net::TcpStream) -> Result<()> {
    // TODO: drive ironrdp-acceptor's handshake here, then hand the negotiated
    // session off to capture + input pipelines. See CLAUDE.md for the layering.
    anyhow::bail!("RDP session handling not yet implemented")
}
