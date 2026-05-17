mod audio;
mod auth;
mod capture;
mod clipboard;
mod cursor;
#[cfg(target_os = "macos")]
mod file_promise;
mod input;

use std::fs;
use std::io::BufReader;
use std::net::SocketAddr;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use der::Decode;
use ironrdp_pdu::rdp::capability_sets::{
    BitmapCodecs, Codec, CodecProperty, NsCodec, RemoteFxContainer,
};
use ironrdp_server::{Credentials, RdpServer};
use rcgen::{generate_simple_self_signed, CertifiedKey};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;
use tracing::{info, warn};
use x509_cert::Certificate;
use zeroize::Zeroizing;

use crate::capture::{primary_display_size, CaptureDisplay};
use crate::input::{ensure_accessibility_access, MacInputHandler};

const FALLBACK_WIDTH: u16 = 1280;
const FALLBACK_HEIGHT: u16 = 720;

/// Codecs advertised to the client. The actual encoder picks the one the
/// client also speaks; on conflict, the negotiation prefers (in order)
/// QOIZ > QOI > RemoteFx > NSCodec.
///
/// NSCodec is here for Microsoft Remote Desktop on macOS, which speaks only
/// NSCodec. The encoder is a Phase-1 stub right now — see CLAUDE.md.
fn bitmap_codecs() -> BitmapCodecs {
    BitmapCodecs(vec![
        // NSCodec — for Microsoft Remote Desktop on macOS.
        Codec {
            id: 0,
            property: CodecProperty::NsCodec(NsCodec {
                is_dynamic_fidelity_allowed: false,
                is_subsampling_allowed: false, // Phase 3 will flip this on.
                color_loss_level: 3,
            }),
        },
        // RemoteFx — what mstsc actually uses.
        Codec {
            id: 0,
            property: CodecProperty::RemoteFx(RemoteFxContainer::ServerContainer(1)),
        },
        Codec {
            id: 0,
            property: CodecProperty::ImageRemoteFx(RemoteFxContainer::ServerContainer(1)),
        },
        // QOI/QOIZ — for FreeRDP and any other client that picks them up.
        Codec {
            id: 0,
            property: CodecProperty::Qoi,
        },
        Codec {
            id: 0,
            property: CodecProperty::QoiZ,
        },
    ])
}

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
    /// Address to bind. Defaults to loopback only — pass `0.0.0.0:3390`
    /// explicitly to accept LAN connections. Port 3389 (the standard RDP
    /// port) is privileged; 3390 is the unprivileged dev default.
    #[arg(long, default_value = "127.0.0.1:3390")]
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

    /// Read the password from the macOS Keychain instead of prompting.
    /// Expects a generic-password entry: service `macrdp`, account = the
    /// resolved username. Create it once with:
    ///   security add-generic-password -s macrdp -a $USER -w
    /// Lets launchd start macrdp without an interactive terminal.
    #[arg(long)]
    keychain: bool,

    /// Directory holding cert.pem / key.pem. Generated on first run and
    /// reused thereafter so clients see a stable fingerprint across restarts.
    /// Defaults to ~/Library/Application Support/macrdp.
    #[arg(long)]
    cert_dir: Option<PathBuf>,

    /// Show debug-level logs from macrdp and the underlying ironrdp / rustls
    /// crates. Without this, known-noisy lines (TLS SNI warnings, the
    /// "Encoding bitmap took N ms" timer, the cert-acceptance "Broken pipe"
    /// from ironrdp_server) are suppressed.
    #[arg(long, short = 'v')]
    verbose: bool,

    /// Don't prevent the Mac from going to sleep / auto-locking while macrdp
    /// is running. By default we spawn `caffeinate` so an idle Mac doesn't
    /// tear down the connection mid-session. Pass this to keep normal power
    /// management on.
    #[arg(long)]
    allow_sleep: bool,
}

/// Prevent macOS from going to sleep, dimming/sleeping the display, idle-
/// locking, or spinning down disks while macrdp is running. `caffeinate
/// -w PID` exits automatically when the supplied PID exits, so there's
/// nothing to clean up on shutdown.
#[cfg(target_os = "macos")]
fn prevent_sleep() {
    let pid = std::process::id().to_string();
    let res = std::process::Command::new("caffeinate")
        .args(["-dimsu", "-w", &pid])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match res {
        Ok(child) => info!(caffeinate_pid = child.id(), "preventing sleep / auto-lock"),
        Err(e) => warn!("could not spawn caffeinate to prevent sleep: {e}"),
    }
}

/// Shell out to `security find-generic-password -s macrdp -a <user> -w`,
/// which prints the password on stdout. The Keychain entry has to be
/// created out-of-band; this never prompts the user interactively.
fn read_password_from_keychain(username: &str) -> Result<Zeroizing<String>> {
    let out = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "macrdp",
            "-a",
            username,
            "-w",
        ])
        .output()
        .context("invoke security(1)")?;
    if !out.status.success() {
        return Err(anyhow!(
            "keychain entry not found (run: security add-generic-password -s macrdp -a {username} -w)"
        ));
    }
    // Wrap as soon as we touch the bytes so the underlying allocation is
    // zeroed when this scope drops. `security` appends a single \n.
    let mut s = Zeroizing::new(
        String::from_utf8(out.stdout).map_err(|_| anyhow!("keychain returned non-UTF8 bytes"))?,
    );
    if s.ends_with('\n') {
        s.pop();
    }
    if s.is_empty() {
        return Err(anyhow!("keychain entry for macrdp is empty"));
    }
    Ok(s)
}

fn default_cert_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join("Library/Application Support/macrdp"))
}

fn load_pem_cert_and_key(
    cert_path: &Path,
    key_path: &Path,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    let cert_file =
        fs::File::open(cert_path).with_context(|| format!("open cert {}", cert_path.display()))?;
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut BufReader::new(cert_file))
        .collect::<std::io::Result<_>>()
        .with_context(|| format!("parse cert {}", cert_path.display()))?;
    if certs.is_empty() {
        return Err(anyhow!("no certificates in {}", cert_path.display()));
    }

    let key_meta =
        fs::metadata(key_path).with_context(|| format!("stat key {}", key_path.display()))?;
    let mode = key_meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(anyhow!(
            "private key {} is group/world-accessible (mode {:o}); refusing to use it. \
             Fix with: chmod 600 {}",
            key_path.display(),
            mode,
            key_path.display(),
        ));
    }

    let key_file =
        fs::File::open(key_path).with_context(|| format!("open key {}", key_path.display()))?;
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

fn make_tls_acceptor(cert_dir: &Path) -> Result<(TlsAcceptor, Vec<u8>)> {
    let cert_path = cert_dir.join("cert.pem");
    let key_path = cert_dir.join("key.pem");

    let (certs, key) = if cert_path.exists() && key_path.exists() {
        info!(dir = %cert_dir.display(), "loading persisted TLS cert");
        load_pem_cert_and_key(&cert_path, &key_path)?
    } else {
        info!(dir = %cert_dir.display(), "generating new self-signed TLS cert");
        generate_and_persist(cert_dir, &cert_path, &key_path)?
    };

    // CredSSP's public-key channel-binding hashes the raw `subjectPublicKey`
    // BIT STRING contents from the X.509 cert — i.e. the DER-encoded
    // RSAPublicKey itself, NOT the full SubjectPublicKeyInfo wrapper. See
    // sspi's `raw_peer_public_key()`. Re-deriving from a keypair, or
    // passing the SPKI sequence, both produce different bytes and the
    // server/client hashes disagree.
    let cert_der_bytes = certs
        .first()
        .ok_or_else(|| anyhow!("empty cert chain"))?
        .as_ref();
    let parsed = Certificate::from_der(cert_der_bytes).context("parse cert DER for SPKI")?;
    let spki_der = parsed
        .tbs_certificate
        .subject_public_key_info
        .subject_public_key
        .raw_bytes()
        .to_vec();

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("build rustls ServerConfig")?;
    Ok((TlsAcceptor::from(Arc::new(config)), spki_der))
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = Args::parse();

    // RUST_LOG (if set) always wins. Otherwise: --verbose turns on debug
    // everywhere; without it we apply a targeted filter that quiets known
    // noisy modules:
    //   - rustls SNI warnings (clients legally put IPs there)
    //   - the ironrdp_server::encoder "took N ms" timer (fires above 10ms)
    //   - ironrdp_server::server WARN ("Unexpected share data pdu"); ERRORs
    //     are still shown — that's where the cert-acceptance "Broken pipe"
    //     comes from, which we can't suppress without losing real errors.
    let filter = if let Ok(env) = tracing_subscriber::EnvFilter::try_from_default_env() {
        env
    } else if args.verbose {
        tracing_subscriber::EnvFilter::new("debug")
    } else {
        tracing_subscriber::EnvFilter::new(
            "info,rustls=error,ironrdp_server::encoder=error,ironrdp_server::server=error",
        )
    };
    // Non-blocking stderr writer: a dedicated background thread drains the
    // tracing channel and writes to stderr, so a slow TTY can't backpressure
    // the tokio runtime when verbose logging is enabled. `_guard` must live
    // for the lifetime of `main` — dropping it flushes and shuts the writer
    // thread down.
    let (nb_writer, _guard) = tracing_appender::non_blocking(std::io::stderr());
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(nb_writer)
        .init();

    // Install signal handling before anything touches ScreenCaptureKit. Once an
    // SCK capture stream is live, macOS framework threads can leave the process
    // unkillable by Ctrl-C; a forced exit on SIGINT/SIGTERM sidesteps that — and
    // any other non-cooperative native threads — instead of relying on the
    // tokio runtime to unwind cleanly.
    tokio::spawn(async {
        shutdown_signal().await;
        info!("shutdown signal received — exiting");
        std::process::exit(0);
    });

    #[cfg(target_os = "macos")]
    {
        if !args.allow_sleep {
            prevent_sleep();
        }
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
    let password: Zeroizing<String> = if args.keychain {
        read_password_from_keychain(&username)?
    } else if let Some(p) = args.password.take() {
        Zeroizing::new(p)
    } else {
        Zeroizing::new(
            rpassword::prompt_password(format!("Password for {username}: "))
                .context("read password from terminal")?,
        )
    };
    if !args.skip_auth {
        auth::authenticate(&username, password.as_str())
            .with_context(|| format!("PAM auth failed for {username}"))?;
        info!(user = %username, "PAM auth ok");
    } else {
        // --skip-auth is a dev-only escape hatch. Refuse it whenever the
        // listener is reachable beyond loopback so a fat-fingered config
        // can't accidentally expose an unauth'd RDP server to the LAN.
        if !args.bind.ip().is_loopback() {
            return Err(anyhow!(
                "--skip-auth refused on non-loopback bind {} — \
                 remove --skip-auth or rebind to 127.0.0.1",
                args.bind,
            ));
        }
        warn!("--skip-auth set; using --password verbatim without PAM check (loopback only)");
    }

    let cert_dir = match args.cert_dir.clone() {
        Some(p) => p,
        None => default_cert_dir()?,
    };
    let (tls, spki_der) = make_tls_acceptor(&cert_dir)?;

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
        info!(
            width,
            height,
            detected_w = dw,
            detected_h = dh,
            "desktop size"
        );
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
    let sound: Box<dyn ironrdp_server::SoundServerFactory> = Box::new(audio::MacRdpsnd::new());

    // with_hybrid advertises HYBRID | HYBRID_EX so clients run CredSSP/NLA
    // over TLS. ironrdp_acceptor's accept_credssp reads our set_credentials
    // and validates the client's NTLM response against it.
    let mut server = RdpServer::builder()
        .with_addr(args.bind)
        .with_hybrid(tls, spki_der)
        .with_input_handler(input_handler)
        .with_display_handler(display)
        .with_cliprdr_factory(Some(cliprdr))
        .with_sound_factory(Some(sound))
        .with_bitmap_codecs(bitmap_codecs())
        .build();

    // ironrdp_server::Credentials holds a plain String, so this copy is
    // outside our control and won't be zeroed when the server shuts down.
    // Our Zeroizing<String> still wipes its own allocation at scope exit.
    server.set_credentials(Some(Credentials {
        username: username.clone(),
        password: (*password).clone(),
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

/// Resolves when the process receives SIGINT (Ctrl-C) or, on Unix, SIGTERM.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut sigterm) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = sigterm.recv() => {}
                }
            }
            Err(e) => {
                warn!("could not install SIGTERM handler: {e}");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
