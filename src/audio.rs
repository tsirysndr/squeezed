//! Audio format description and the SlimProto format-code mapping.
//!
//! SlimProto's `strm` command describes a raw-PCM stream with four single-byte
//! ASCII codes (sample size, sample rate, channels, endianness). Squeezelite
//! decodes each by subtracting `'0'` and indexing a fixed table, so the codes
//! are just small integers rendered as ASCII digits (and beyond `'9'` the
//! arithmetic keeps working, e.g. `':'` → 10).

/// Sample-rate table used by Squeezelite's PCM decoder. The index into this
/// table, offset by `'0'`, is the ASCII code carried in the `strm` command.
const SAMPLE_RATES: [u32; 15] = [
    11025, 22050, 32000, 44100, 48000, 8000, 12000, 16000, 24000, 96000, 88200, 176400, 192000,
    352800, 384000,
];

/// A fully described raw-PCM stream: signed little-endian by default, which is
/// exactly what `ffmpeg -f s16le` and friends emit.
#[derive(Clone, Copy, Debug)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u8,
    pub bits: u8,
}

impl AudioFormat {
    /// Bytes per PCM frame (all channels, one sample each).
    pub fn frame_bytes(&self) -> usize {
        (self.bits as usize / 8) * self.channels as usize
    }

    /// Byte rate — useful for reasoning about buffer sizes and latency.
    pub fn byte_rate(&self) -> usize {
        self.frame_bytes() * self.sample_rate as usize
    }

    /// SlimProto sample-size code: `'0'`=8, `'1'`=16, `'2'`=24, `'3'`=32 bit.
    pub fn sample_size_code(&self) -> anyhow::Result<u8> {
        match self.bits {
            8 => Ok(b'0'),
            16 => Ok(b'1'),
            24 => Ok(b'2'),
            32 => Ok(b'3'),
            other => anyhow::bail!("unsupported bit depth: {other} (expected 8, 16, 24 or 32)"),
        }
    }

    /// SlimProto sample-rate code: index into [`SAMPLE_RATES`] offset by `'0'`.
    pub fn sample_rate_code(&self) -> anyhow::Result<u8> {
        match SAMPLE_RATES.iter().position(|&r| r == self.sample_rate) {
            Some(i) => Ok(b'0' + i as u8),
            None => anyhow::bail!(
                "unsupported sample rate: {} Hz (supported: {:?})",
                self.sample_rate,
                SAMPLE_RATES
            ),
        }
    }

    /// SlimProto channel code: `'1'`=mono, `'2'`=stereo, …
    pub fn channels_code(&self) -> anyhow::Result<u8> {
        match self.channels {
            1 | 2 => Ok(b'0' + self.channels),
            other => anyhow::bail!("unsupported channel count: {other} (expected 1 or 2)"),
        }
    }

    /// SlimProto endianness code. We always emit little-endian PCM: `'1'`=LE.
    pub fn endianness_code(&self) -> u8 {
        b'1'
    }

    /// Validate every field up front so misconfiguration fails at startup
    /// rather than when the first client connects.
    pub fn validate(&self) -> anyhow::Result<()> {
        self.sample_size_code()?;
        self.sample_rate_code()?;
        self.channels_code()?;
        Ok(())
    }
}
