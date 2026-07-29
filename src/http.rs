//! Minimal HTTP/1.0 server that streams the shared PCM buffer.
//!
//! When Squeezelite receives our `strm` command it opens an HTTP connection
//! here and reads raw PCM until end of stream. Each connection gets its own
//! [`BroadcastReceiver`], so any number of clients play concurrently.

use crate::broadcast::{BroadcastBuffer, RecvResult};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

pub fn serve(bind_ip: &str, port: u16, buf: Arc<BroadcastBuffer>) -> anyhow::Result<()> {
    let listener = TcpListener::bind((bind_ip, port))
        .map_err(|e| anyhow::anyhow!("http: bind {bind_ip}:{port} failed: {e}"))?;
    tracing::info!("http: streaming PCM on {bind_ip}:{port}");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let buf = Arc::clone(&buf);
                std::thread::spawn(move || handle(stream, buf));
            }
            Err(e) => tracing::warn!("http: accept error: {e}"),
        }
    }
    Ok(())
}

fn handle(mut stream: TcpStream, buf: Arc<BroadcastBuffer>) {
    let peer = stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_default();

    if let Err(e) = drain_request(&mut stream) {
        tracing::warn!("http: request read error from {peer}: {e}");
        return;
    }

    // Content-Type is advisory only: in raw-PCM mode Squeezelite takes the
    // format from the `strm` command, not from these headers.
    let headers = b"HTTP/1.0 200 OK\r\n\
        Content-Type: application/octet-stream\r\n\
        Cache-Control: no-cache\r\n\
        Connection: close\r\n\
        \r\n";
    if let Err(e) = stream.write_all(headers) {
        tracing::warn!("http: header write error to {peer}: {e}");
        return;
    }

    tracing::info!("http: client {peer} attached to stream");
    let mut rx = buf.subscribe();
    loop {
        match rx.recv_blocking() {
            RecvResult::Data(chunk) => {
                if stream.write_all(&chunk).is_err() {
                    tracing::info!("http: client {peer} disconnected");
                    break;
                }
            }
            RecvResult::Closed => {
                tracing::info!("http: stream closed, dropping {peer}");
                break;
            }
        }
    }
}

/// Read and discard the HTTP request head up to the blank-line terminator.
fn drain_request(stream: &mut TcpStream) -> std::io::Result<()> {
    let mut buf: Vec<u8> = Vec::with_capacity(512);
    let mut byte = [0u8; 1];
    loop {
        stream.read_exact(&mut byte)?;
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") || buf.ends_with(b"\n\n") || buf.len() > 8192 {
            return Ok(());
        }
    }
}
