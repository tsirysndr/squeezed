//! SlimProto TCP server (port 3483 by default).
//!
//! Each Squeezelite instance connects, sends `HELO`, and we reply with a `strm`
//! "start" command describing the raw-PCM format and pointing it at our HTTP
//! endpoint (with its MAC in the URL so the HTTP side can correlate the two
//! connections). A per-client thread then sends `strm 't'` once per second: this
//! both keeps Squeezelite's watchdog quiet and solicits a `STMt` timing report,
//! which we feed to the [`SyncManager`] for multiroom alignment.

use crate::audio::AudioFormat;
use crate::sync::{self, SyncManager};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub fn serve(
    bind_ip: &str,
    slim_port: u16,
    http_port: u16,
    format: AudioFormat,
    manager: Arc<SyncManager>,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind((bind_ip, slim_port))
        .map_err(|e| anyhow::anyhow!("slim: bind {bind_ip}:{slim_port} failed: {e}"))?;
    tracing::info!("slim: SlimProto listening on {bind_ip}:{slim_port}");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let manager = Arc::clone(&manager);
                std::thread::spawn(move || handle_client(stream, http_port, format, manager));
            }
            Err(e) => tracing::warn!("slim: accept error: {e}"),
        }
    }
    Ok(())
}

fn handle_client(
    mut stream: TcpStream,
    http_port: u16,
    format: AudioFormat,
    manager: Arc<SyncManager>,
) {
    let peer = stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_default();
    tracing::info!("slim: client connected from {peer}");

    let (mac, name) = match read_client_packet(&mut stream) {
        Ok((op, body)) if op == "HELO" => {
            let mac = parse_helo_mac(&body);
            let name = parse_helo_name(&body).unwrap_or_else(|| mac.clone());
            tracing::info!("slim: HELO from {peer} (name={name:?}, mac={mac})");
            (mac, name)
        }
        Ok((op, _)) => {
            tracing::warn!("slim: expected HELO from {peer}, got {op:?}");
            return;
        }
        Err(e) => {
            tracing::debug!("slim: read error from {peer}: {e}");
            return;
        }
    };

    // Write half, shared by the strm/audg sends, the probe ticker, and the sync
    // engine's corrections. Both fds refer to the same socket (POSIX-safe).
    let write_stream = match stream.try_clone() {
        Ok(s) => Arc::new(Mutex::new(s)),
        Err(e) => {
            tracing::error!("slim: try_clone failed for {peer}: {e}");
            return;
        }
    };

    manager.add_player(mac.clone(), name.clone(), Arc::clone(&write_stream));

    // Start playback: describe the format and hand the client its HTTP URL,
    // tagged with the MAC so the HTTP server can link the connections.
    {
        let mut s = write_stream.lock().unwrap();
        if let Err(e) = send_strm_start(&mut s, http_port, &format, &mac) {
            tracing::error!("slim: sending strm to {peer} failed: {e}");
            manager.remove_player(&mac);
            return;
        }
        let _ = send_audg(&mut s); // unity gain, once
    }
    tracing::info!("slim: sent strm to {peer} → HTTP audio on :{http_port}");

    // Probe ticker: `strm 't'` every second (timing solicitation + watchdog).
    {
        let ws = Arc::clone(&write_stream);
        let peer = peer.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(1));
            let mut s = ws.lock().unwrap();
            if send_strm_command(&mut s, b't', sync::server_ms()).is_err() {
                tracing::debug!("slim: probe write to {peer} failed");
                break;
            }
        });
    }

    // Read loop: turn STMt timing reports into sync updates; handle disconnect.
    loop {
        match read_client_packet(&mut stream) {
            Ok((op, body)) if op == "STAT" && body.len() >= 4 => {
                let ev = std::str::from_utf8(&body[..4]).unwrap_or("????");
                if ev == "STMt" {
                    let recv_ms = sync::server_ms();
                    let jiffies = read_u32_be(&body, 25);
                    let elapsed_ms = read_u32_be(&body, 43);
                    let server_ts = read_u32_be(&body, 47);
                    manager.on_stmt(&mac, jiffies, elapsed_ms, server_ts, recv_ms);
                } else {
                    tracing::debug!("slim: STAT {ev} from {peer}");
                }
            }
            Ok((op, _)) if op == "DSCO" => {
                tracing::info!("slim: DSCO from {peer}");
                break;
            }
            Ok((op, _)) => tracing::debug!("slim: {op} from {peer}"),
            Err(_) => {
                tracing::info!("slim: {peer} disconnected");
                break;
            }
        }
    }
    manager.remove_player(&mac);
}

// ---------------------------------------------------------------------------
// Packet I/O
// ---------------------------------------------------------------------------

/// Client → server framing: 4-byte opcode, 4-byte big-endian length, payload.
fn read_client_packet(stream: &mut TcpStream) -> std::io::Result<(String, Vec<u8>)> {
    let mut opcode = [0u8; 4];
    stream.read_exact(&mut opcode)?;
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut payload = vec![0u8; len];
    if len > 0 {
        stream.read_exact(&mut payload)?;
    }
    Ok((String::from_utf8_lossy(&opcode).into_owned(), payload))
}

/// Server → client framing: 2-byte big-endian length (opcode + payload),
/// 4-byte opcode, payload.
fn send_server_packet(
    stream: &mut TcpStream,
    opcode: &[u8; 4],
    payload: &[u8],
) -> std::io::Result<()> {
    let total = 4 + payload.len();
    stream.write_all(&(total as u16).to_be_bytes())?;
    stream.write_all(opcode)?;
    stream.write_all(payload)?;
    Ok(())
}

fn send_strm_start(
    stream: &mut TcpStream,
    http_port: u16,
    format: &AudioFormat,
    mac: &str,
) -> std::io::Result<()> {
    // MAC in the query string lets the HTTP server correlate this player's two
    // connections (SlimProto control + HTTP audio).
    let request = format!("GET /stream.pcm?player={mac} HTTP/1.0\r\n\r\n");
    // Format codes are validated at startup, so unwrap here is safe.
    let sample_size = format.sample_size_code().unwrap();
    let sample_rate = format.sample_rate_code().unwrap();
    let channels = format.channels_code().unwrap();
    let endianness = format.endianness_code();

    let mut payload = Vec::with_capacity(24 + request.len());
    payload.push(b's'); // command: start
    payload.push(b'1'); // autostart: play once buffered; the sync engine aligns
    payload.push(b'p'); // format: raw PCM
    payload.push(sample_size);
    payload.push(sample_rate);
    payload.push(channels);
    payload.push(endianness);
    payload.push(255); // in-threshold (KB) before autostart
    payload.push(0); // spdif_enable
    payload.push(0); // transition_period
    payload.push(b'0'); // transition_type: none
    payload.push(0); // flags
    payload.push(0); // output_threshold
    payload.push(0); // slaves
    payload.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // replay_gain = 1.0
    payload.extend_from_slice(&http_port.to_be_bytes());
    payload.extend_from_slice(&0u32.to_be_bytes()); // server_ip = 0 → reuse slimproto IP
    payload.extend_from_slice(request.as_bytes());
    send_server_packet(stream, b"strm", &payload)
}

/// Send a bare `strm` control command (`t`ickle / `a`skip / `p`ause / `u`npause)
/// whose `replay_gain` field carries `value` (a timestamp, interval, or skip).
pub(crate) fn send_strm_command(
    stream: &mut TcpStream,
    command: u8,
    value: u32,
) -> std::io::Result<()> {
    let mut payload = [0u8; 24];
    payload[0] = command;
    payload[14..18].copy_from_slice(&value.to_be_bytes()); // replay_gain
    send_server_packet(stream, b"strm", &payload)
}

/// `audg` — unity gain. Sent once so playback isn't muted.
fn send_audg(stream: &mut TcpStream) -> std::io::Result<()> {
    let mut payload = [0u8; 9];
    payload[0..4].copy_from_slice(&0x0001_0000u32.to_be_bytes()); // left = 1.0
    payload[4..8].copy_from_slice(&0x0001_0000u32.to_be_bytes()); // right = 1.0
    send_server_packet(stream, b"audg", &payload)
}

fn read_u32_be(data: &[u8], offset: usize) -> u32 {
    if data.len() < offset + 4 {
        return 0;
    }
    u32::from_be_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ])
}

// ---------------------------------------------------------------------------
// HELO body parsers (device_id, revision, mac[6], uuid[16], … capabilities)
// ---------------------------------------------------------------------------

fn parse_helo_mac(body: &[u8]) -> String {
    if body.len() < 8 {
        return "000000000000".into();
    }
    let m = &body[2..8];
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        m[0], m[1], m[2], m[3], m[4], m[5]
    )
}

fn parse_helo_name(body: &[u8]) -> Option<String> {
    const CAP_OFFSET: usize = 36;
    if body.len() <= CAP_OFFSET {
        return None;
    }
    let caps = std::str::from_utf8(&body[CAP_OFFSET..]).ok()?;
    for part in caps.trim_end_matches('\0').split(',') {
        if let Some(name) = part.strip_prefix("Name=") {
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}
