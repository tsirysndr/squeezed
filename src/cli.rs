//! Command-line interface (clap derive).
//!
//! All value flags are `Option<T>` so that "unset on the command line" is
//! distinguishable from "set to the default" — [`crate::config::Config`] uses
//! that to layer defaults ← TOML ← flags with correct precedence.

use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "squeezed",
    version,
    about = "Serve a raw PCM audio stream to any Squeezelite/Squeezebox client over SlimProto.",
    long_about = "squeezed reads a raw PCM (default S16LE) stream from stdin, a FIFO, a unix \
socket, or a TCP socket, and serves it over the SlimProto protocol so any Squeezelite client \
can play it. Configure via CLI flags and/or a TOML file (flags win)."
)]
pub struct Cli {
    /// Path to a TOML configuration file (CLI flags override its values).
    #[arg(short, long, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Input source: stdin | fifo | unix | tcp.
    #[arg(short, long, value_name = "SOURCE")]
    pub source: Option<String>,

    /// Path for the `fifo` or `unix` source.
    #[arg(long, value_name = "PATH")]
    pub path: Option<String>,

    /// Bind address for the `tcp` source (e.g. 0.0.0.0:4711).
    #[arg(long, value_name = "ADDR")]
    pub tcp_bind: Option<String>,

    /// PCM sample rate in Hz (e.g. 44100, 48000).
    #[arg(long, value_name = "HZ")]
    pub sample_rate: Option<u32>,

    /// PCM channel count (1 or 2).
    #[arg(long, value_name = "N")]
    pub channels: Option<u8>,

    /// PCM bit depth (8, 16, 24 or 32).
    #[arg(long, value_name = "BITS")]
    pub bits: Option<u8>,

    /// Address the servers bind to (default 0.0.0.0).
    #[arg(long, value_name = "IP")]
    pub bind_ip: Option<String>,

    /// SlimProto TCP port (default 3483).
    #[arg(long, value_name = "PORT")]
    pub slim_port: Option<u16>,

    /// HTTP audio port squeezelite streams PCM from (default 9000).
    #[arg(long, value_name = "PORT")]
    pub http_port: Option<u16>,

    /// Enable/disable UDP service discovery (default enabled).
    #[arg(long, value_name = "BOOL", num_args = 0..=1, default_missing_value = "true")]
    pub discovery: Option<bool>,

    /// Enable/disable multiroom synchronization of connected players (default enabled).
    #[arg(long, value_name = "BOOL", num_args = 0..=1, default_missing_value = "true")]
    pub sync: Option<bool>,

    /// Server name advertised over discovery / HELO.
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,

    /// Rolling PCM buffer size in bytes (retention window).
    #[arg(long, value_name = "BYTES")]
    pub buffer_bytes: Option<usize>,
}
