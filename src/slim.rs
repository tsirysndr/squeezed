//! SlimProto TCP server (port 3483 by default).
//!
//! Each Squeezelite instance connects, sends `HELO`, and we reply with a
//! `strm` "start" command describing the raw-PCM format and pointing it at our
//! HTTP endpoint. A per-client sync thread forwards server jiffies so multiple
//! clients converge on the same playback clock, and every `STMt` heartbeat is
//! answered with `audg` to keep Squeezelite's 36-second watchdog quiet.

use crate::audio::AudioFormat;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Fans a single jiffies value out to every connected client once per second.
pub struct SyncBroadcaster {
    senders: Mutex<Vec<mpsc::Sender<u32>>>,
}

impl SyncBroadcaster {
    pub fn new() -> Arc<Self> {
        Arc::new(SyncBroadcaster {
            senders: Mutex::new(Vec::new()),
        })
    }

    fn subscribe(&self) -> mpsc::Receiver<u32> {
        let (tx, rx) = mpsc::channel();
        self.senders.lock().unwrap().push(tx);
        rx
    }

    fn broadcast(&self, jiffies: u32) {
        // Dropping a dead sender prunes the corresponding disconnected client.
        self.senders.lock().unwrap().retain(|tx| tx.send(jiffies).is_ok());
    }
}

/// Spawn the once-per-second jiffies fan-out loop.
pub fn spawn_sync_ticker(sync: Arc<SyncBroadcaster>) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(1));
        sync.broadcast(server_jiffies());
    });
}

/// Milliseconds since the Unix epoch, truncated to u32 (~49-day rollover).
fn server_jiffies() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u32
}

pub fn serve(
    bind_ip: &str,
    slim_port: u16,
    http_port: u16,
    format: AudioFormat,
    sync: Arc<SyncBroadcaster>,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind((bind_ip, slim_port))
        .map_err(|e| anyhow::anyhow!("slim: bind {bind_ip}:{slim_port} failed: {e}"))?;
    tracing::info!("slim: SlimProto listening on {bind_ip}:{slim_port}");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                // Subscribe before spawning so the client is registered before
                // the next jiffies broadcast fires.
                let sync_rx = sync.subscribe();
                std::thread::spawn(move || handle_client(stream, http_port, format, sync_rx));
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
    sync_rx: mpsc::Receiver<u32>,
) {
    let peer = stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_default();
    tracing::info!("slim: client connected from {peer}");

    match read_client_packet(&mut stream) {
        Ok((op, body)) if op == "HELO" => {
            let name = parse_helo_name(&body).unwrap_or_else(|| peer.clone());
            tracing::info!("slim: HELO from {peer} (name={name:?}, mac={})", parse_helo_mac(&body));
        }
        Ok((op, _)) => {
            tracing::warn!("slim: expected HELO from {peer}, got {op:?}");
            return;
        }
        Err(e) => {
            tracing::debug!("slim: read error from {peer}: {e}");
            return;
        }
    }

    if let Err(e) = send_strm_start(&mut stream, http_port, &format) {
        tracing::error!("slim: sending strm to {peer} failed: {e}");
        return;
    }
    tracing::info!("slim: sent strm to {peer} → HTTP audio on :{http_port}");

    // One fd for writes (shared with the sync thread), one for reads. Both
    // refer to the same socket; POSIX makes concurrent read/write safe.
    let write_stream = match stream.try_clone() {
        Ok(s) => Arc::new(Mutex::new(s)),
        Err(e) => {
            tracing::error!("slim: try_clone failed for {peer}: {e}");
            return;
        }
    };

    // Sync writer thread: forward server jiffies to this client.
    {
        let ws = Arc::clone(&write_stream);
        let peer = peer.clone();
        std::thread::spawn(move || {
            for jiffies in sync_rx {
                let mut s = ws.lock().unwrap();
                if send_sync(&mut s, jiffies).is_err() {
                    tracing::debug!("slim: sync write to {peer} failed");
                    break;
                }
            }
        });
    }

    // Read loop: answer STMt heartbeats with audg, handle disconnect.
    loop {
        match read_client_packet(&mut stream) {
            Ok((op, body)) if op == "STAT" && body.len() >= 4 => {
                let ev = std::str::from_utf8(&body[..4]).unwrap_or("????");
                if ev == "STMt" {
                    let mut s = write_stream.lock().unwrap();
                    if send_audg(&mut s).is_err() {
                        break;
                    }
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
fn send_server_packet(stream: &mut TcpStream, opcode: &[u8; 4], payload: &[u8]) -> std::io::Result<()> {
    let total = 4 + payload.len();
    stream.write_all(&(total as u16).to_be_bytes())?;
    stream.write_all(opcode)?;
    stream.write_all(payload)?;
    Ok(())
}

fn send_strm_start(stream: &mut TcpStream, http_port: u16, format: &AudioFormat) -> std::io::Result<()> {
    let request = b"GET /stream.pcm HTTP/1.0\r\n\r\n";
    // Format codes are validated at startup, so unwrap here is safe.
    let sample_size = format.sample_size_code().unwrap();
    let sample_rate = format.sample_rate_code().unwrap();
    let channels = format.channels_code().unwrap();
    let endianness = format.endianness_code();

    let mut payload = Vec::with_capacity(24 + request.len());
    payload.push(b's'); // command: start
    payload.push(b'1'); // autostart
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
    payload.extend_from_slice(request);
    send_server_packet(stream, b"strm", &payload)
}

/// `audg` — unity gain. Sent on each heartbeat to suppress the watchdog.
fn send_audg(stream: &mut TcpStream) -> std::io::Result<()> {
    let mut payload = [0u8; 9];
    payload[0..4].copy_from_slice(&0x0001_0000u32.to_be_bytes()); // left = 1.0
    payload[4..8].copy_from_slice(&0x0001_0000u32.to_be_bytes()); // right = 1.0
    send_server_packet(stream, b"audg", &payload)
}

/// `sync` — align the client's playback clock to `jiffies`.
fn send_sync(stream: &mut TcpStream, jiffies: u32) -> std::io::Result<()> {
    send_server_packet(stream, b"sync", &jiffies.to_be_bytes())
}

// ---------------------------------------------------------------------------
// HELO body parsers (device_id, revision, mac[6], uuid[16], … capabilities)
// ---------------------------------------------------------------------------

fn parse_helo_mac(body: &[u8]) -> String {
    if body.len() < 8 {
        return "unknown".into();
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
