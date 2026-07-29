//! `squeezed` — serve a raw PCM stream to Squeezelite/Squeezebox over SlimProto.
//!
//! Wiring: the input source pumps PCM into a shared broadcast buffer; the
//! SlimProto server tells each client to open an HTTP connection; the HTTP
//! server streams the buffer to every client; a UDP responder makes the server
//! discoverable. The input pump runs on the main thread — when it ends (stdin
//! EOF) the buffer is closed and the process exits.

mod audio;
mod broadcast;
mod cli;
mod config;
mod discovery;
mod http;
mod input;
mod slim;
mod sync;

use clap::Parser;
use cli::Cli;
use config::Config;
use std::sync::Arc;

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let cfg = Config::resolve(&cli)?;
    init_tracing();

    let retention_s = cfg.buffer_bytes as f64 / cfg.format.byte_rate().max(1) as f64;
    tracing::info!(
        "squeezed {}: name={:?}, input={}, {} Hz / {} ch / {}-bit, slim :{}, http :{}, \
         discovery={}, sync={}, buffer={} KiB (~{:.1}s)",
        env!("CARGO_PKG_VERSION"),
        cfg.server_name,
        config::describe_source(&cfg.source),
        cfg.format.sample_rate,
        cfg.format.channels,
        cfg.format.bits,
        cfg.slim_port,
        cfg.http_port,
        cfg.discovery,
        cfg.sync,
        cfg.buffer_bytes / 1024,
        retention_s,
    );

    let buf = broadcast::BroadcastBuffer::new(cfg.buffer_bytes);
    let manager = sync::SyncManager::new(cfg.format, cfg.sync);

    // HTTP audio server.
    {
        let buf = Arc::clone(&buf);
        let manager = Arc::clone(&manager);
        let bind_ip = cfg.bind_ip.clone();
        let port = cfg.http_port;
        spawn_named("http", move || {
            if let Err(e) = http::serve(&bind_ip, port, buf, manager) {
                fatal(e);
            }
        });
    }

    // SlimProto control server (drives per-client timing + sync corrections).
    {
        let bind_ip = cfg.bind_ip.clone();
        let (slim_port, http_port, format) = (cfg.slim_port, cfg.http_port, cfg.format);
        let manager = Arc::clone(&manager);
        spawn_named("slim", move || {
            if let Err(e) = slim::serve(&bind_ip, slim_port, http_port, format, manager) {
                fatal(e);
            }
        });
    }

    // UDP discovery responder (optional).
    if cfg.discovery {
        let bind_ip = cfg.bind_ip.clone();
        let (port, name, http_port) = (cfg.slim_port, cfg.server_name.clone(), cfg.http_port);
        spawn_named("discovery", move || {
            if let Err(e) = discovery::serve(&bind_ip, port, &name, http_port) {
                // Discovery is best-effort — a bind failure (e.g. port in use)
                // shouldn't take the whole server down.
                tracing::warn!("{e}");
            }
        });
    }

    // Pump the input on the main thread; when it returns, drain and exit.
    input::run(&cfg.source, Arc::clone(&buf))?;
    buf.close();
    tracing::info!("squeezed: input ended, shutting down");
    Ok(())
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_env("SQUEEZED_LOG")
        .or_else(|_| EnvFilter::try_new("info"))
        .unwrap();
    fmt().with_env_filter(filter).with_target(false).init();
}

fn spawn_named<F: FnOnce() + Send + 'static>(name: &str, f: F) {
    let _ = std::thread::Builder::new().name(name.to_string()).spawn(f);
}

/// A server thread failing to even start (e.g. a port is taken) is
/// unrecoverable — surface it and exit rather than silently degrade.
fn fatal(e: anyhow::Error) -> ! {
    tracing::error!("{e}");
    std::process::exit(1);
}
