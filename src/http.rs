//! Minimal HTTP/1.0 server that streams the shared PCM buffer.
//!
//! When Squeezelite receives our `strm` command it opens an HTTP connection
//! here (with `?player=<mac>` in the URL) and reads raw PCM until end of stream.
//! Each connection gets its own [`BroadcastReceiver`]; we also record the
//! absolute byte position of the live head at attach time and hand it to the
//! [`SyncManager`] as this player's anchor.

use crate::broadcast::{BroadcastBuffer, RecvResult};
use crate::sync::SyncManager;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

pub fn serve(
    bind_ip: &str,
    port: u16,
    buf: Arc<BroadcastBuffer>,
    manager: Arc<SyncManager>,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind((bind_ip, port))
        .map_err(|e| anyhow::anyhow!("http: bind {bind_ip}:{port} failed: {e}"))?;
    tracing::info!("http: streaming PCM on {bind_ip}:{port}");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let buf = Arc::clone(&buf);
                let manager = Arc::clone(&manager);
                std::thread::spawn(move || handle(stream, buf, manager));
            }
            Err(e) => tracing::warn!("http: accept error: {e}"),
        }
    }
    Ok(())
}

fn handle(mut stream: TcpStream, buf: Arc<BroadcastBuffer>, manager: Arc<SyncManager>) {
    let peer = stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_default();

    let request_line = match read_request(&mut stream) {
        Ok(line) => line,
        Err(e) => {
            tracing::warn!("http: request read error from {peer}: {e}");
            return;
        }
    };

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

    // Anchor this player at the current live-head byte position, then subscribe.
    // Reading total_pushed just before subscribe keeps the anchor and the read
    // cursor within one chunk of each other.
    let b_start = buf.total_pushed();
    let mut rx = buf.subscribe();
    if let Some(mac) = parse_player_mac(&request_line) {
        manager.set_http_start(&mac, b_start);
    }

    tracing::info!("http: client {peer} attached to stream");
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

/// Read the HTTP request head, returning the request line (first line).
fn read_request(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut buf: Vec<u8> = Vec::with_capacity(512);
    let mut byte = [0u8; 1];
    loop {
        stream.read_exact(&mut byte)?;
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") || buf.ends_with(b"\n\n") || buf.len() > 8192 {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf);
    Ok(text.lines().next().unwrap_or("").to_string())
}

/// Extract `<mac>` from a request line like `GET /stream.pcm?player=<mac> HTTP/1.0`.
fn parse_player_mac(request_line: &str) -> Option<String> {
    let path = request_line.split_whitespace().nth(1)?;
    let query = path.split_once('?')?.1;
    for pair in query.split('&') {
        if let Some(mac) = pair.strip_prefix("player=") {
            if !mac.is_empty() {
                return Some(mac.to_string());
            }
        }
    }
    None
}
