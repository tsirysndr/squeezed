# squeezed

**Serve a raw PCM audio stream to any [Squeezelite](https://github.com/ralph-irving/squeezelite) / Squeezebox client over the SlimProto protocol.**

`squeezed` takes a raw **PCM S16LE** audio stream from **stdin**, a **FIFO**, a **unix socket**, or a **TCP socket**, and turns it into a Squeezebox server. Point any Squeezelite instance at it — or let them auto-discover it — and the audio comes out the other end, in sync across as many players as you like.

```
┌────────────┐   PCM    ┌───────────────────────────────────┐  SlimProto   ┌──────────────┐
│  ffmpeg /  │ ───────► │ squeezed                          │ ◄──────────► │ squeezelite  │
│  any PCM   │  stdin / │  • SlimProto control  (tcp :3483) │   (tcp)      │  (player 1)  │
│  producer  │  fifo /  │  • HTTP audio stream  (tcp :9000) │  HTTP audio  ├──────────────┤
│            │  unix /  │  • UDP discovery      (udp :3483) │ ───────────► │ squeezelite  │
└────────────┘  tcp     └───────────────────────────────────┘              │  (player 2)  │
                                                                           └──────────────┘
```

---

## Features

- **Any PCM source** — read from `stdin`, a named pipe, a unix socket, or a TCP listener.
- **Configurable format** — sample rate, channel count, and bit depth (8/16/24/32).
- **Zero-config playback** — answers SlimProto UDP discovery so clients find the server automatically; or point them at it with `-s host`.
- **Multiple synchronized clients** — every player gets its own cursor into a shared rolling buffer, plus a per-second clock `sync`.
- **Configurable everything** — CLI flags and/or a TOML file, with clear precedence (defaults ← file ← flags).
- **Tiny & dependency-light** — a single static-ish binary; no LMS, no Perl, no database.

---

## Install

Build from source (needs a recent [Rust toolchain](https://rustup.rs)):

```bash
git clone https://github.com/tsirysndr/squeezed
cd squeezed
cargo build --release
# binary at ./target/release/squeezed
```

Optionally drop it on your `PATH`:

```bash
install -m755 target/release/squeezed /usr/local/bin/squeezed
```

You'll also want a PCM producer ([`ffmpeg`](https://ffmpeg.org)) and a player ([`squeezelite`](https://github.com/ralph-irving/squeezelite)) — both are in most package managers (`brew install ffmpeg squeezelite`, `apt install ffmpeg squeezelite`, …).

---

## Quick start — pipe from ffmpeg, play with squeezelite

**Terminal 1 — start the server and feed it audio from ffmpeg:**

```bash
ffmpeg -re -i song.flac -f s16le -ar 44100 -ac 2 - | squeezed
```

- `-re` streams at real-time rate (so playback isn't a fast-forward blur).
- `-f s16le -ar 44100 -ac 2 -` emits raw signed 16-bit little-endian stereo PCM to stdout, which `squeezed` reads from stdin.

**Terminal 2 — start a player. It auto-discovers `squeezed`:**

```bash
squeezelite -n Living-Room
```

…or skip discovery and point it straight at the server:

```bash
squeezelite -n Living-Room -s 127.0.0.1
```

That's it — `song.flac` now plays through squeezelite. Start more `squeezelite` instances (on this or other machines) and they'll all play in sync.

> **Tip:** to verify the plumbing without an audio device, have squeezelite decode to stdout:
> `squeezelite -s 127.0.0.1 -o - -a 16 > out.pcm`

---

## Input sources

Select the source with `--source` (or `[input] source` in the TOML file).

### stdin (default)

Best for pipelines. EOF on stdin cleanly shuts the server down.

```bash
ffmpeg -re -i input.mp3 -f s16le -ar 44100 -ac 2 - | squeezed --source stdin
```

### FIFO (named pipe)

The server stays up across producers — when one writer closes, `squeezed` waits for the next.

```bash
mkfifo /tmp/squeezed.fifo
squeezed --source fifo --path /tmp/squeezed.fifo &

# feed it whenever you like; the server keeps running between tracks
ffmpeg -re -i track1.flac -f s16le -ar 44100 -ac 2 - > /tmp/squeezed.fifo
ffmpeg -re -i track2.flac -f s16le -ar 44100 -ac 2 - > /tmp/squeezed.fifo
```

### Unix domain socket

Like a FIFO, but connection-oriented. `squeezed` creates (and cleans up) the socket.

```bash
squeezed --source unix --path /tmp/squeezed.sock &
ffmpeg -re -i input.wav -f s16le -ar 44100 -ac 2 - | socat - UNIX-CONNECT:/tmp/squeezed.sock
```

### TCP socket

Feed audio over the network. `squeezed` accepts one writer at a time and waits for the next.

```bash
squeezed --source tcp --tcp-bind 0.0.0.0:4711 &

# from anywhere on the network:
ffmpeg -re -i input.opus -f s16le -ar 44100 -ac 2 - | nc server-host 4711
```

---

## Configuration

Options can come from **CLI flags**, a **TOML file** (`--config`), or both. Precedence, lowest to highest:

```
built-in defaults  <  --config file  <  command-line flags
```

### TOML file

Copy [`squeezed.example.toml`](squeezed.example.toml) and edit. Every key is optional; a partial file only overrides what it sets.

```toml
[input]
source = "fifo"                 # stdin | fifo | unix | tcp
path   = "/tmp/squeezed.fifo"   # for fifo/unix
# bind = "0.0.0.0:4711"         # for tcp

[audio]
sample_rate = 44100             # 8000, 11025, 16000, 22050, 24000, 32000, 44100, 48000, 88200, 96000, ...
channels    = 2                 # 1 or 2
bits        = 16                # 8, 16, 24 or 32

[server]
bind_ip   = "0.0.0.0"
slim_port = 3483                # SlimProto control + UDP discovery
http_port = 9000                # HTTP audio stream
discovery = true                # answer UDP discovery
name      = "squeezed"          # device/server name advertised to clients
# buffer_bytes = 4194304        # rolling PCM retention window (~23s @ 44.1k/16/2)
```

Run it:

```bash
squeezed --config squeezed.example.toml
# override individual values on top of the file:
squeezed --config squeezed.example.toml --http-port 9001 --name Kitchen
```

### CLI reference

| Flag | TOML key | Default | Description |
|------|----------|---------|-------------|
| `-c, --config <FILE>` | — | — | Load a TOML config file (flags still win). |
| `-s, --source <SRC>` | `input.source` | `stdin` | `stdin` \| `fifo` \| `unix` \| `tcp`. |
| `--path <PATH>` | `input.path` | — | Path for `fifo` / `unix`. |
| `--tcp-bind <ADDR>` | `input.bind` | `0.0.0.0:4711` | Bind address for `tcp`. |
| `--sample-rate <HZ>` | `audio.sample_rate` | `44100` | PCM sample rate. |
| `--channels <N>` | `audio.channels` | `2` | 1 (mono) or 2 (stereo). |
| `--bits <BITS>` | `audio.bits` | `16` | 8, 16, 24 or 32. |
| `--bind-ip <IP>` | `server.bind_ip` | `0.0.0.0` | Interface to listen on. |
| `--slim-port <PORT>` | `server.slim_port` | `3483` | SlimProto control + UDP discovery port. |
| `--http-port <PORT>` | `server.http_port` | `9000` | HTTP audio port. |
| `--discovery <BOOL>` | `server.discovery` | `true` | Answer UDP discovery. |
| `--name <NAME>` | `server.name` | `squeezed` | Device/server name. |
| `--buffer-bytes <N>` | `server.buffer_bytes` | `4194304` | Rolling PCM retention window. |

Logging verbosity is controlled by the `SQUEEZED_LOG` environment variable (`error`, `warn`, `info`, `debug`, `trace`; default `info`):

```bash
SQUEEZED_LOG=debug squeezed --source stdin < /dev/null
```

---

## Audio format

The stream **must** be raw, signed, little-endian PCM matching the configured `sample_rate` / `channels` / `bits`. That's exactly what ffmpeg's `-f s16le` (or `s24le`/`s32le`), `-ar <rate>`, `-ac <n>` produce. Mismatched format = wrong-speed or noisy playback, so keep the ffmpeg flags and the `[audio]` settings in agreement.

| bits | ffmpeg format |
|------|---------------|
| 16   | `-f s16le`    |
| 24   | `-f s24le`    |
| 32   | `-f s32le`    |

Supported sample rates: `8000, 11025, 12000, 16000, 22050, 24000, 32000, 44100, 48000, 88200, 96000, 176400, 192000, 352800, 384000` Hz.

Example — 48 kHz / 24-bit:

```bash
ffmpeg -re -i hi-res.flac -f s24le -ar 48000 -ac 2 - | squeezed --sample-rate 48000 --bits 24
```

---

## Discovery & multiple players

- **Discovery** — with `discovery = true` (default), `squeezed` answers SlimProto UDP discovery on the SlimProto port, so `squeezelite` finds it with no `-s`. Because Squeezelite hard-codes discovery to port **3483**, auto-discovery only works while `slim_port` is left at the default; with a custom port, point clients at the server explicitly (`-s host:port`).
- **Multiple players** — every connected player gets an independent cursor into a shared rolling buffer and a once-per-second clock `sync`, so they stay aligned. A player that stalls briefly is caught up from the buffer; a player that falls further behind than the retention window skips forward rather than stalling the stream.

```bash
squeezelite -n Kitchen      -s 192.168.1.10 &
squeezelite -n Living-Room  -s 192.168.1.10 &
squeezelite -n Bedroom      -s 192.168.1.10 &
```

---

## More recipes

**Re-broadcast an internet radio stream:**

```bash
ffmpeg -re -i https://stream.example.com/radio.mp3 -f s16le -ar 44100 -ac 2 - | squeezed
```

**Play a whole folder, gaplessly, over a FIFO:**

```bash
mkfifo /tmp/squeezed.fifo
squeezed --source fifo --path /tmp/squeezed.fifo &
for f in ~/Music/*.flac; do
  ffmpeg -re -i "$f" -f s16le -ar 44100 -ac 2 - 
done > /tmp/squeezed.fifo
```

**Stream your desktop/system audio (PipeWire/PulseAudio via ffmpeg):**

```bash
ffmpeg -f pulse -i default -f s16le -ar 44100 -ac 2 - | squeezed
```

**Stream microphone/line-in on macOS (avfoundation):**

```bash
ffmpeg -f avfoundation -i ":0" -f s16le -ar 44100 -ac 2 - | squeezed
```

**Generate a test tone:**

```bash
ffmpeg -re -f lavfi -i "sine=frequency=440:sample_rate=44100" -ac 2 -f s16le - | squeezed
```

---

## Troubleshooting

- **No players show up / auto-discovery fails** — pass `-s <server-ip>` to squeezelite. Discovery needs `slim_port` at its default `3483` and UDP broadcast reachable on your LAN.
- **`bind … failed: Address already in use`** — something already owns the port (often a running Logitech Media Server on `3483`). Pick another with `--slim-port` / `--http-port` (and then point players with `-s host:port`).
- **Playback is a fast-forward blur** — you forgot `-re` on ffmpeg; without it, ffmpeg pushes PCM as fast as it can. `-re` paces it to real time.
- **Noise / wrong pitch** — the ffmpeg output format doesn't match `[audio]`. Make `-f s16le -ar <rate> -ac <n>` agree with `bits` / `sample_rate` / `channels`.
- **See what's happening** — run with `SQUEEZED_LOG=debug`.

---

## How it works

1. **SlimProto server** (TCP, default `:3483`) accepts each Squeezelite connection, reads its `HELO`, and replies with a `strm` "start" command describing the raw-PCM format and telling the client to fetch audio over HTTP.
2. **HTTP server** (TCP, default `:9000`) streams the shared PCM buffer to each connected player.
3. **Input pump** reads PCM from the configured source into a one-writer / N-reader rolling **broadcast buffer**.
4. **Sync + discovery** — a once-per-second `sync` keeps players aligned, and a UDP responder makes the server discoverable.

The SlimProto implementation is derived from the [`rockbox-slim`](https://github.com/tsirysndr/rockbox-zig) crate.

---

## License

[MIT](LICENSE) © Tsiry Sandratraina
