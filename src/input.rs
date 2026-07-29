//! PCM input sources.
//!
//! Every source ultimately hands us a byte stream that we pump into the
//! [`BroadcastBuffer`] in fixed-size chunks. `stdin` is one-shot (EOF ends the
//! program); `fifo`, `unix`, and `tcp` are long-lived — when a writer
//! disconnects we wait for the next one and keep the SlimProto clients attached.

use crate::broadcast::BroadcastBuffer;
use crate::config::InputSource;
use std::io::Read;
use std::net::TcpListener;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::Arc;

/// Read size per pump iteration. 16 KiB keeps latency low while amortizing the
/// per-chunk locking/allocation cost.
const CHUNK: usize = 16 * 1024;

/// Run the configured input source, pumping PCM into `buf` until the source is
/// permanently exhausted (only `stdin` returns; the others loop forever).
pub fn run(source: &InputSource, buf: Arc<BroadcastBuffer>) -> anyhow::Result<()> {
    match source {
        InputSource::Stdin => {
            tracing::info!("input: reading PCM from stdin");
            let stdin = std::io::stdin();
            pump(stdin.lock(), &buf);
            tracing::info!("input: stdin reached EOF");
            Ok(())
        }
        InputSource::Fifo { path } => run_fifo(path, buf),
        InputSource::Unix { path } => run_unix(path, buf),
        InputSource::Tcp { bind } => run_tcp(bind, buf),
    }
}

fn run_fifo(path: &str, buf: Arc<BroadcastBuffer>) -> anyhow::Result<()> {
    if !Path::new(path).exists() {
        anyhow::bail!(
            "input: FIFO {path} does not exist — create it first with `mkfifo {path}`"
        );
    }
    tracing::info!("input: reading PCM from FIFO {path}");
    loop {
        // Opening a FIFO for reading blocks until a writer appears; when the
        // writer closes we hit EOF, then loop to await the next writer.
        match std::fs::File::open(path) {
            Ok(f) => {
                pump(f, &buf);
                tracing::info!("input: FIFO writer disconnected, awaiting next writer");
            }
            Err(e) => anyhow::bail!("input: opening FIFO {path} failed: {e}"),
        }
    }
}

fn run_unix(path: &str, buf: Arc<BroadcastBuffer>) -> anyhow::Result<()> {
    // A stale socket file from a previous run would make bind() fail.
    if Path::new(path).exists() {
        let _ = std::fs::remove_file(path);
    }
    let listener = UnixListener::bind(path)
        .map_err(|e| anyhow::anyhow!("input: binding unix socket {path} failed: {e}"))?;
    tracing::info!("input: reading PCM from unix socket {path}");
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                tracing::info!("input: unix writer connected");
                pump(stream, &buf);
                tracing::info!("input: unix writer disconnected");
            }
            Err(e) => tracing::warn!("input: unix accept error: {e}"),
        }
    }
    Ok(())
}

fn run_tcp(bind: &str, buf: Arc<BroadcastBuffer>) -> anyhow::Result<()> {
    let listener = TcpListener::bind(bind)
        .map_err(|e| anyhow::anyhow!("input: binding tcp {bind} failed: {e}"))?;
    tracing::info!("input: reading PCM from tcp {bind}");
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
                tracing::info!("input: tcp writer {peer} connected");
                pump(stream, &buf);
                tracing::info!("input: tcp writer {peer} disconnected");
            }
            Err(e) => tracing::warn!("input: tcp accept error: {e}"),
        }
    }
    Ok(())
}

/// Copy `reader` into the broadcast buffer until EOF or a read error.
fn pump<R: Read>(mut reader: R, buf: &Arc<BroadcastBuffer>) {
    let mut chunk = vec![0u8; CHUNK];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break, // EOF
            Ok(n) => buf.push(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                tracing::warn!("input: read error: {e}");
                break;
            }
        }
    }
}
