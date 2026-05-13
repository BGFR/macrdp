mod audio;
mod auth;
mod capture;
mod clipboard;
mod cursor;
mod input;

use std::fs;
use std::io::BufReader;
use std::net::SocketAddr;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use ironrdp_server::{Credentials, RdpServer};
use rcgen::{generate_simple_self_signed, CertifiedKey};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;
use tracing::{info, warn};

use crate::capture::{primary_display_size, CaptureDisplay};
use crate::input::{ensure_accessibility_access, MacInputHandler};

const FALLBACK_WIDTH: u16 = 1280;
const FALLBACK_HEIGHT: u16 = 720;

#[cfg(target_os = "macos")]
fn ensure_screen_recording_access() {
    use core_graphics::access::ScreenCaptureAccess;
    let tcc = ScreenCaptureAccess;
    if tcc.preflight() {
        info!("Screen Recording permission already granted");
        return;
    }
    warn!(
        "Screen Recording permission NOT granted. macrdp will appear in \
         System Settings → Privacy & Security → Screen Recording. Enable it, \
         then RESTART macrdp (TCC grants only take effect on next launch)."
    );
    // request() registers the binary with TCC and opens the prompt; the
    // returned bool reflects current state, which is still false on first run.
    let _ = tcc.request();
}

#[derive(Parser, Debug)]
#[command(name = "macrdp", about = "Native RDP server for macOS")]
struct Args {
    /// Address to bind. Default is unprivileged 3390; real RDP is 3389.
    #[arg(long, default_value = "0.0.0.0:3390")]
    bind: SocketAddr,

    /// Desktop width in pixels. Defaults to the primary display's native width
    /// (queried via ScreenCaptureKit). Set to override scaling.
    #[arg(long)]
    width: Option<u16>,

    /// Desktop height in pixels. Defaults to the primary display's native height.
    #[arg(long)]
    height: Option<u16>,

    #[arg(long, default_value_t = 15)]
    fps: u32,

    /// Mac account the client authenticates as. Defaults to $USER. The
    /// password is validated against the local account via PAM (checkpw
    /// service) at startup, so this must be a real Mac user.
    #[arg(long)]
    username: Option<String>,

    /// Password to use without an interactive prompt. Discouraged — leaving
    /// this unset and entering at the prompt avoids shell-history leakage.
    #[arg(long)]
    password: Option<String>,

    /// Skip the PAM check and use the supplied --password verbatim. Useful
    /// for non-macOS dev or scripted tests; never use on a shared network.
    #[arg(long)]
    skip_auth: bool,

    /// Directory holding cert.pem / key.pem. Generated on first run and
    /// reused thereafter so clients see a stable fingerprint across restarts.
    /// Defaults to ~/Library/Application Support/macrdp.
    #[arg(long)]
    cert_dir: Option<PathBuf>,
}

fn default_cert_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join("Library/Application Support/macrdp"))
}

fn load_pem_cert_and_key(
    cert_path: &Path,
    key_path: &Path,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    let cert_file = fs::File::open(cert_path)
        .with_context(|| format!("open cert {}", cert_path.display()))?;
    let certs: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut BufReader::new(cert_file))
            .collect::<std::io::Result<_>>()
            .with_context(|| format!("parse cert {}", cert_path.display()))?;
    if certs.is_empty() {
        return Err(anyhow!("no certificates in {}", cert_path.display()));
    }

    let key_meta = fs::metadata(key_path)
        .with_context(|| format!("stat key {}", key_path.display()))?;
    let mode = key_meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        warn!(
            path = %key_path.display(),
            mode = format!("{:o}", mode),
            "private key is group/world-accessible; chmod 600 it"
        );
    }

    let key_file = fs::File::open(key_path)
        .with_context(|| format!("open key {}", key_path.display()))?;
    let key = rustls_pemfile::private_key(&mut BufReader::new(key_file))
        .with_context(|| format!("parse key {}", key_path.display()))?
        .ok_or_else(|| anyhow!("no private key in {}", key_path.display()))?;

    Ok((certs, key))
}

fn generate_and_persist(
    cert_dir: &Path,
    cert_path: &Path,
    key_path: &Path,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    fs::create_dir_all(cert_dir)
        .with_context(|| format!("create cert dir {}", cert_dir.display()))?;

    let CertifiedKey { cert, key_pair } =
        generate_simple_self_signed(vec!["localhost".to_string()])
            .context("generate self-signed cert")?;

    let cert_pem = cert.pem();
    let key_pem = key_pair.serialize_pem();

    fs::write(cert_path, &cert_pem)
        .with_context(|| format!("write cert {}", cert_path.display()))?;

    // Create key with 0600 from the start — never let it briefly exist 0644.
    {
        use std::io::Write as _;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(key_path)
            .with_context(|| format!("create key {}", key_path.display()))?;
        f.write_all(key_pem.as_bytes())
            .with_context(|| format!("write key {}", key_path.display()))?;
    }
    // If the file pre-existed with looser perms, OpenOptions::mode is a no-op
    // on truncate — re-assert 0600 explicitly.
    fs::set_permissions(key_path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod key {}", key_path.display()))?;

    let cert_der = CertificateDer::from(cert.der().to_vec());
    let key_der = PrivateKeyDer::try_from(key_pair.serialize_der())
        .map_err(|e| anyhow!("convert key DER: {e}"))?;
    Ok((vec![cert_der], key_der))
}

fn make_tls_acceptor(cert_dir: &Path) -> Result<TlsAcceptor> {
    let cert_path = cert_dir.join("cert.pem");
    let key_path = cert_dir.join("key.pem");

    let (certs, key) = if cert_path.exists() && key_path.exists() {
        info!(dir = %cert_dir.display(), "loading persisted TLS cert");
        load_pem_cert_and_key(&cert_path, &key_path)?
    } else {
        info!(dir = %cert_dir.display(), "generating new self-signed TLS cert");
        generate_and_persist(cert_dir, &cert_path, &key_path)?
    };

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("build rustls ServerConfig")?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    #[cfg(target_os = "macos")]
    {
        ensure_screen_recording_access();
        if !ensure_accessibility_access() {
            warn!(
                "Accessibility permission NOT granted. macrdp will appear in \
                 System Settings → Privacy & Security → Accessibility. Enable \
                 it, then RESTART macrdp. Without it, keyboard/mouse input \
                 from RDP clients is silently dropped."
            );
        } else {
            info!("Accessibility permission already granted");
        }
    }
    #[cfg(not(target_os = "macos"))]
    tracing::warn!("Built for a non-macOS target — capture is a static-rectangle stub.");

    let username = args
        .username
        .clone()
        .or_else(|| std::env::var("USER").ok())
        .ok_or_else(|| anyhow!("no username: pass --username or set $USER"))?;
    let password = match args.password.clone() {
        Some(p) => p,
        None => rpassword::prompt_password(format!("Password for {username}: "))
            .context("read password from terminal")?,
    };
    if !args.skip_auth {
        auth::authenticate(&username, &password)
            .with_context(|| format!("PAM auth failed for {username}"))?;
        info!(user = %username, "PAM auth ok");
    } else {
        warn!("--skip-auth set; using --password verbatim without PAM check");
    }

    let cert_dir = match args.cert_dir.clone() {
        Some(p) => p,
        None => default_cert_dir()?,
    };
    let tls = make_tls_acceptor(&cert_dir)?;

    let detected = primary_display_size().await?;
    let width = args
        .width
        .or(detected.map(|(w, _)| w))
        .unwrap_or(FALLBACK_WIDTH);
    let height = args
        .height
        .or(detected.map(|(_, h)| h))
        .unwrap_or(FALLBACK_HEIGHT);
    if let Some((dw, dh)) = detected {
        info!(width, height, detected_w = dw, detected_h = dh, "desktop size");
    } else {
        info!(width, height, "desktop size (no display detected)");
    }

    let display = CaptureDisplay {
        width,
        height,
        fps: args.fps,
    };

    let input_handler = MacInputHandler::new(width, height)?;
    let cliprdr: Box<dyn ironrdp_server::CliprdrServerFactory> =
        Box::new(clipboard::MacCliprdr::new());
    let sound: Box<dyn ironrdp_server::SoundServerFactory> =
        Box::new(audio::MacRdpsnd::new());

    let mut server = RdpServer::builder()
        .with_addr(args.bind)
        .with_tls(tls)
        .with_input_handler(input_handler)
        .with_display_handler(display)
        .with_cliprdr_factory(Some(cliprdr))
        .with_sound_factory(Some(sound))
        .build();

    server.set_credentials(Some(Credentials {
        username: username.clone(),
        password,
        domain: None,
    }));

    info!(
        addr = %args.bind,
        user = %username,
        "macrdp listening — connect with: mstsc / xfreerdp at port {} as {}",
        args.bind.port(),
        username,
    );
    server.run().await
}
