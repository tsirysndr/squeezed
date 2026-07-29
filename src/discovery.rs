//! SlimProto UDP service discovery responder.
//!
//! Squeezelite (and Squeezebox hardware), when started without an explicit
//! `-s <server>`, broadcasts a UDP discovery datagram to port 3483 and connects
//! SlimProto to whoever answers. We listen on the same UDP port and reply so
//! clients find `squeezed` with zero configuration.
//!
//! Two request forms are handled:
//!   * TLV "get" (`'e'` …) — the modern form squeezelite uses. The body is a
//!     list of 4-byte tag + 1-byte length + value entries (length 0 in a
//!     request). We answer with `'E'` and the same tags filled in.
//!   * Legacy (`'d'`) — old hardware discovery; answered with a bare `'D'`.
//!
//! Note: squeezelite hard-codes the discovery/SlimProto port to 3483, so
//! auto-discovery only works when `slim_port` is left at its default. With a
//! custom port, point clients at the server explicitly (`-s host:port`).

use std::net::UdpSocket;

pub fn serve(bind_ip: &str, port: u16, name: &str, http_port: u16) -> anyhow::Result<()> {
    let socket = UdpSocket::bind((bind_ip, port))
        .map_err(|e| anyhow::anyhow!("discovery: bind {bind_ip}:{port}/udp failed: {e}"))?;
    tracing::info!("discovery: responding to SlimProto discovery on {bind_ip}:{port}/udp");

    let version = env!("CARGO_PKG_VERSION");
    let json_port = http_port.to_string();
    let mut buf = [0u8; 1500];

    loop {
        let (n, from) = match socket.recv_from(&mut buf) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("discovery: recv error: {e}");
                continue;
            }
        };
        let req = &buf[..n];
        let Some(&kind) = req.first() else { continue };

        let reply = match kind {
            b'e' => {
                let tags = parse_requested_tags(&req[1..]);
                tracing::debug!("discovery: TLV request from {from} tags={tags:?}");
                Some(build_tlv_response(&tags, name, version, &json_port))
            }
            b'd' => {
                tracing::debug!("discovery: legacy request from {from}");
                Some(vec![b'D'])
            }
            _ => None,
        };

        if let Some(reply) = reply {
            if let Err(e) = socket.send_to(&reply, from) {
                tracing::warn!("discovery: reply to {from} failed: {e}");
            } else {
                tracing::info!("discovery: answered {from}");
            }
        }
    }
}

/// Parse the tag names from a TLV "get" body (each entry: tag[4] len[1] value[len]).
fn parse_requested_tags(body: &[u8]) -> Vec<[u8; 4]> {
    let mut tags = Vec::new();
    let mut i = 0;
    while i + 5 <= body.len() {
        let tag = [body[i], body[i + 1], body[i + 2], body[i + 3]];
        let len = body[i + 4] as usize;
        tags.push(tag);
        i += 5 + len;
    }
    tags
}

/// Build an `'E'` TLV response, answering each requested tag we recognise.
fn build_tlv_response(tags: &[[u8; 4]], name: &str, version: &str, json_port: &str) -> Vec<u8> {
    let mut out = vec![b'E'];
    // Always advertise NAME and JSON even if the client didn't list them —
    // squeezelite keys on these to identify and reach the server.
    let mut answered: Vec<[u8; 4]> = Vec::new();
    let push = |out: &mut Vec<u8>, answered: &mut Vec<[u8; 4]>, tag: &[u8; 4], value: &[u8]| {
        if answered.contains(tag) {
            return;
        }
        let len = value.len().min(255);
        out.extend_from_slice(tag);
        out.push(len as u8);
        out.extend_from_slice(&value[..len]);
        answered.push(*tag);
    };

    for tag in tags {
        match tag {
            b"NAME" => push(&mut out, &mut answered, tag, name.as_bytes()),
            b"JSON" => push(&mut out, &mut answered, tag, json_port.as_bytes()),
            b"VERS" => push(&mut out, &mut answered, tag, version.as_bytes()),
            b"JVID" | b"UUID" => push(&mut out, &mut answered, tag, b""),
            _ => {}
        }
    }
    // Guarantee the essentials are present.
    push(&mut out, &mut answered, b"NAME", name.as_bytes());
    push(&mut out, &mut answered, b"JSON", json_port.as_bytes());

    out
}
