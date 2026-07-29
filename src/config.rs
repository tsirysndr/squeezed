//! Configuration model plus the merge of defaults ← TOML file ← CLI flags.
//!
//! Every CLI flag is optional so we can layer sources with clear precedence:
//! start from [`Config::default`], overlay a TOML file if given, then overlay
//! any flags the user actually passed.

use crate::audio::AudioFormat;
use crate::cli::Cli;
use serde::Deserialize;

/// Where PCM comes from.
#[derive(Clone, Debug)]
pub enum InputSource {
    /// Standard input (one-shot; EOF ends the program).
    Stdin,
    /// Named pipe at `path` (create with `mkfifo`).
    Fifo { path: String },
    /// Unix domain socket at `path`.
    Unix { path: String },
    /// TCP listener at `bind` (e.g. `0.0.0.0:4711`).
    Tcp { bind: String },
}

/// Fully-resolved runtime configuration.
#[derive(Clone, Debug)]
pub struct Config {
    pub source: InputSource,
    pub format: AudioFormat,
    pub bind_ip: String,
    pub slim_port: u16,
    pub http_port: u16,
    pub discovery: bool,
    pub server_name: String,
    pub buffer_bytes: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            source: InputSource::Stdin,
            format: AudioFormat {
                sample_rate: 44100,
                channels: 2,
                bits: 16,
            },
            bind_ip: "0.0.0.0".to_string(),
            slim_port: 3483,
            http_port: 9000,
            discovery: true,
            server_name: "squeezed".to_string(),
            buffer_bytes: crate::broadcast::MAX_BUFFERED,
        }
    }
}

// --- TOML schema -----------------------------------------------------------
// Mirrors squeezed.example.toml. Every field is optional so a partial file
// only overrides the keys it sets.

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    #[serde(default)]
    input: FileInput,
    #[serde(default)]
    audio: FileAudio,
    #[serde(default)]
    server: FileServer,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileInput {
    /// "stdin" | "fifo" | "unix" | "tcp"
    source: Option<String>,
    /// Path for fifo/unix sources.
    path: Option<String>,
    /// Bind address for the tcp source.
    bind: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileAudio {
    sample_rate: Option<u32>,
    channels: Option<u8>,
    bits: Option<u8>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileServer {
    bind_ip: Option<String>,
    slim_port: Option<u16>,
    http_port: Option<u16>,
    discovery: Option<bool>,
    name: Option<String>,
    buffer_bytes: Option<usize>,
}

impl Config {
    /// Resolve the final configuration from CLI flags and an optional file.
    pub fn resolve(cli: &Cli) -> anyhow::Result<Config> {
        let mut cfg = Config::default();

        if let Some(path) = &cli.config {
            let text = std::fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("reading config {}: {e}", path.display()))?;
            let file: FileConfig = toml::from_str(&text)
                .map_err(|e| anyhow::anyhow!("parsing config {}: {e}", path.display()))?;
            cfg.apply_file(file)?;
        }

        cfg.apply_cli(cli)?;
        cfg.format.validate()?;
        Ok(cfg)
    }

    fn apply_file(&mut self, f: FileConfig) -> anyhow::Result<()> {
        if let Some(rate) = f.audio.sample_rate {
            self.format.sample_rate = rate;
        }
        if let Some(ch) = f.audio.channels {
            self.format.channels = ch;
        }
        if let Some(bits) = f.audio.bits {
            self.format.bits = bits;
        }
        if let Some(ip) = f.server.bind_ip {
            self.bind_ip = ip;
        }
        if let Some(p) = f.server.slim_port {
            self.slim_port = p;
        }
        if let Some(p) = f.server.http_port {
            self.http_port = p;
        }
        if let Some(d) = f.server.discovery {
            self.discovery = d;
        }
        if let Some(n) = f.server.name {
            self.server_name = n;
        }
        if let Some(b) = f.server.buffer_bytes {
            self.buffer_bytes = b;
        }
        if let Some(src) = f.input.source {
            self.source = build_source(&src, f.input.path.as_deref(), f.input.bind.as_deref())?;
        }
        Ok(())
    }

    fn apply_cli(&mut self, cli: &Cli) -> anyhow::Result<()> {
        if let Some(rate) = cli.sample_rate {
            self.format.sample_rate = rate;
        }
        if let Some(ch) = cli.channels {
            self.format.channels = ch;
        }
        if let Some(bits) = cli.bits {
            self.format.bits = bits;
        }
        if let Some(ip) = &cli.bind_ip {
            self.bind_ip = ip.clone();
        }
        if let Some(p) = cli.slim_port {
            self.slim_port = p;
        }
        if let Some(p) = cli.http_port {
            self.http_port = p;
        }
        if let Some(d) = cli.discovery {
            self.discovery = d;
        }
        if let Some(n) = &cli.name {
            self.server_name = n.clone();
        }
        if let Some(b) = cli.buffer_bytes {
            self.buffer_bytes = b;
        }
        if let Some(src) = &cli.source {
            self.source = build_source(src, cli.path.as_deref(), cli.tcp_bind.as_deref())?;
        }
        Ok(())
    }
}

/// Turn a source keyword plus its path/bind operands into an [`InputSource`].
fn build_source(source: &str, path: Option<&str>, bind: Option<&str>) -> anyhow::Result<InputSource> {
    match source {
        "stdin" | "-" => Ok(InputSource::Stdin),
        "fifo" => {
            let path = path
                .ok_or_else(|| anyhow::anyhow!("input source 'fifo' requires a path (--path)"))?;
            Ok(InputSource::Fifo { path: path.to_string() })
        }
        "unix" => {
            let path = path
                .ok_or_else(|| anyhow::anyhow!("input source 'unix' requires a path (--path)"))?;
            Ok(InputSource::Unix { path: path.to_string() })
        }
        "tcp" => {
            let bind = bind.unwrap_or("0.0.0.0:4711");
            Ok(InputSource::Tcp { bind: bind.to_string() })
        }
        other => anyhow::bail!("unknown input source '{other}' (expected stdin, fifo, unix or tcp)"),
    }
}

/// Human-readable one-liner describing where PCM will come from.
pub fn describe_source(source: &InputSource) -> String {
    match source {
        InputSource::Stdin => "stdin".to_string(),
        InputSource::Fifo { path } => format!("fifo {path}"),
        InputSource::Unix { path } => format!("unix {path}"),
        InputSource::Tcp { bind } => format!("tcp {bind}"),
    }
}
